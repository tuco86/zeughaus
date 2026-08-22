use crate::context::{InputSet, NodeContext};
use crate::convert::TypeConverters;
use crate::error::Result;
use crate::node::{NodeDefinition, SettingDef};
use crate::pin::{PinBinding, PinDefinition};
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

    /// Recomputes this node's pins from what is currently connected to its
    /// inputs. Returns true if the pin set changed, so the host re-syncs pin
    /// definitions and redraws. Default: pins are static.
    ///
    /// A variadic node reads only the binding names (a merge node grows an input
    /// once the last one fills). A node whose shape follows its data -- a table
    /// writer taking a schema, a subgraph exposing its interior -- reads the
    /// incoming types.
    fn sync_pins(&mut self, _connected: &[PinBinding<'_>]) -> bool {
        false
    }
}
