use zeughaus_core::*;

/// Remaps a value from [in_min, in_max] to [out_min, out_max].
/// Formula: out_min + (value - in_min) / (in_max - in_min) * (out_max - out_min)
pub struct MapRangeNode {
    pins: Vec<PinDefinition>,
}

impl Default for MapRangeNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MapRangeNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("value", Ty::Float, PinKind::Trigger),
                PinDefinition::input("in_min", Ty::Float, PinKind::Sample),
                PinDefinition::input("in_max", Ty::Float, PinKind::Sample),
                PinDefinition::input("out_min", Ty::Float, PinKind::Sample),
                PinDefinition::input("out_max", Ty::Float, PinKind::Sample),
                PinDefinition::output("result", Ty::Float),
            ],
        }
    }
}

impl ExecutableNode for MapRangeNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let value: f64 = inputs.get("value").unwrap_or(0.0);
        let in_min: f64 = inputs.get("in_min").unwrap_or(0.0);
        let in_max: f64 = inputs.get("in_max").unwrap_or(1.0);
        let out_min: f64 = inputs.get("out_min").unwrap_or(0.0);
        let out_max: f64 = inputs.get("out_max").unwrap_or(1.0);

        let in_range = in_max - in_min;
        let result = if in_range.abs() < 1e-15 {
            out_min
        } else {
            let t = (value - in_min) / in_range;
            out_min + t * (out_max - out_min)
        };

        ctx.emit_typed("result", result);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_0_to_1_onto_0_to_100() {
        let mut node = MapRangeNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("value", Value::new(0.5f64));
        inputs.insert("in_min", Value::new(0.0f64));
        inputs.insert("in_max", Value::new(1.0f64));
        inputs.insert("out_min", Value::new(0.0f64));
        inputs.insert("out_max", Value::new(100.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&50.0));
    }

    #[test]
    fn maps_celsius_to_fahrenheit() {
        let mut node = MapRangeNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("value", Value::new(100.0f64));
        inputs.insert("in_min", Value::new(0.0f64));
        inputs.insert("in_max", Value::new(100.0f64));
        inputs.insert("out_min", Value::new(32.0f64));
        inputs.insert("out_max", Value::new(212.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&212.0));
    }

    #[test]
    fn zero_range_returns_out_min() {
        let mut node = MapRangeNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("value", Value::new(5.0f64));
        inputs.insert("in_min", Value::new(5.0f64));
        inputs.insert("in_max", Value::new(5.0f64));
        inputs.insert("out_min", Value::new(10.0f64));
        inputs.insert("out_max", Value::new(20.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&10.0));
    }
}
