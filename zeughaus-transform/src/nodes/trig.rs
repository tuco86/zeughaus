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
    use zeughaus_core::*;

    fn exec_unary(node: &mut dyn ExecutableNode, input: f64) -> f64 {
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(input));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        *ctx.take_outputs()["result"].downcast_ref::<f64>().unwrap()
    }

    #[test]
    fn sin_of_pi_is_zero() {
        assert!(exec_unary(&mut SinNode::new(), PI).abs() < 1e-10);
    }

    #[test]
    fn cos_of_zero_is_one() {
        assert!((exec_unary(&mut CosNode::new(), 0.0) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn sqrt_of_25() {
        assert_eq!(exec_unary(&mut SqrtNode::new(), 25.0), 5.0);
    }

    #[test]
    fn floor_of_3_7() {
        assert_eq!(exec_unary(&mut FloorNode::new(), 3.7), 3.0);
    }

    #[test]
    fn ceil_of_3_2() {
        assert_eq!(exec_unary(&mut CeilNode::new(), 3.2), 4.0);
    }

    #[test]
    fn round_of_3_5() {
        assert_eq!(exec_unary(&mut RoundNode::new(), 3.5), 4.0);
    }
}
