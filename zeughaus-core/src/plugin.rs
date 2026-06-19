use crate::context::{InputSet, NodeContext};
use crate::error::Result;
use crate::node::{NodeDefinition, SettingDef};
use crate::pin::PinDefinition;
use crate::value::Value;

pub trait DomainPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn node_catalog(&self) -> Vec<NodeDefinition>;
    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>>;
}

pub trait ExecutableNode: Send + Sync {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()>;
    fn pin_definitions(&self) -> &[PinDefinition];

    /// Editable in-node text settings. Each maps to a `set_parameter` key.
    fn settings(&self) -> Vec<SettingDef> {
        Vec::new()
    }

    fn set_parameter(&mut self, _name: &str, _value: Value) -> Result<()> {
        Ok(())
    }
}
