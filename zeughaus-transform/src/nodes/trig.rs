use zeughaus_core::*;

macro_rules! unary_f64_node {
    ($name:ident, $op:expr) => {
        pub struct $name {
            pins: Vec<PinDefinition>,
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    pins: vec![
                        PinDefinition {
                            name: "input",
                            direction: PinDirection::Input,
                            data_mode: DataMode::Value,
                            pin_kind: PinKind::Trigger,
                            type_name: "f64",
                        },
                        PinDefinition {
                            name: "result",
                            direction: PinDirection::Output,
                            data_mode: DataMode::Value,
                            pin_kind: PinKind::Sample,
                            type_name: "f64",
                        },
                    ],
                }
            }
        }

        impl ExecutableNode for $name {
            fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
                let v: f64 = inputs.get("input").unwrap_or(0.0);
                let op: fn(f64) -> f64 = $op;
                ctx.emit_typed("result", op(v));
                ctx.flush();
                Ok(())
            }

            fn pin_definitions(&self) -> &[PinDefinition] {
                &self.pins
            }
        }
    };
}

unary_f64_node!(SinNode, f64::sin);
unary_f64_node!(CosNode, f64::cos);
unary_f64_node!(TanNode, f64::tan);
unary_f64_node!(SqrtNode, f64::sqrt);
unary_f64_node!(FloorNode, f64::floor);
unary_f64_node!(CeilNode, f64::ceil);
unary_f64_node!(RoundNode, f64::round);
unary_f64_node!(Log2Node, f64::log2);
unary_f64_node!(LnNode, f64::ln);

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn exec_unary(node: &mut dyn ExecutableNode, input: f64) -> f64 {
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(input));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        *outputs["result"].downcast_ref::<f64>().unwrap()
    }

    #[test]
    fn sin_of_pi_is_zero() {
        let mut node = SinNode::new();
        let result = exec_unary(&mut node, PI);
        assert!(result.abs() < 1e-10);
    }

    #[test]
    fn cos_of_zero_is_one() {
        let mut node = CosNode::new();
        let result = exec_unary(&mut node, 0.0);
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    fn sqrt_of_25() {
        let mut node = SqrtNode::new();
        let result = exec_unary(&mut node, 25.0);
        assert_eq!(result, 5.0);
    }

    #[test]
    fn floor_of_3_7() {
        let mut node = FloorNode::new();
        let result = exec_unary(&mut node, 3.7);
        assert_eq!(result, 3.0);
    }

    #[test]
    fn ceil_of_3_2() {
        let mut node = CeilNode::new();
        let result = exec_unary(&mut node, 3.2);
        assert_eq!(result, 4.0);
    }

    #[test]
    fn round_of_3_5() {
        let mut node = RoundNode::new();
        let result = exec_unary(&mut node, 3.5);
        assert_eq!(result, 4.0);
    }
}
