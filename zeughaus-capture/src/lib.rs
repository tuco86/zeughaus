pub mod nodes;

use zeughaus_core::*;

use nodes::*;

pub struct CapturePlugin;

impl DomainPlugin for CapturePlugin {
    fn name(&self) -> &str {
        "capture"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![catalog_entry(
            "capture.screen",
            "Screen Capture",
            "Capture",
            &ScreenCaptureNode::new(),
        )]
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
