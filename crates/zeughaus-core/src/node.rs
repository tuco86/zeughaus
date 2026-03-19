use crate::pin::PinDefinition;

#[derive(Debug, Clone, Default)]
pub struct NodeConfig {
    pub capture: bool,
}

#[derive(Debug, Clone)]
pub struct NodeDefinition {
    pub type_id: &'static str,
    pub display_name: &'static str,
    pub category: &'static str,
    pub pins: Vec<PinDefinition>,
}
