pub mod nodes;

use zeughaus_core::*;

use nodes::*;

pub struct CapturePlugin;

impl DomainPlugin for CapturePlugin {
    fn name(&self) -> &str {
        "capture"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![NodeDefinition {
            type_id: "capture.screen",
            display_name: "Screen Capture",
            category: "Capture",
            pins: ScreenCaptureNode::new().pin_definitions().to_vec(),
        }]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "capture.screen" => Some(Box::new(ScreenCaptureNode::new())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_screen_capture() {
        let plugin = CapturePlugin;
        assert_eq!(plugin.node_catalog().len(), 1);
        assert_eq!(plugin.node_catalog()[0].type_id, "capture.screen");
    }

    #[test]
    fn create_screen_capture() {
        let plugin = CapturePlugin;
        assert!(plugin.create_node("capture.screen").is_some());
    }
}
