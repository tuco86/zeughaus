use crate::context::{InputSet, NodeContext};
use crate::convert::TypeConverters;
use crate::error::Result;
use crate::node::{NodeDefinition, SettingDef};
use crate::pin::PinDefinition;
use crate::value::Value;

pub trait DomainPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn node_catalog(&self) -> Vec<NodeDefinition>;
    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>>;

    /// Registers the type converters this plugin provides, so its pin types can
    /// interoperate with other plugins' types across edges. Default: none.
    fn register_converters(&self, _converters: &mut TypeConverters) {}
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

    /// Recomputes a variadic node's input pins from the names of its currently
    /// connected input pins. Returns true if the pin set changed, so the host
    /// re-syncs pin definitions and redraws. Default: pins are static.
    fn sync_arity(&mut self, _connected_inputs: &[&str]) -> bool {
        false
    }
}
