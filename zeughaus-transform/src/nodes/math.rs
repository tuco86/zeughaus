binary_f64_node!(AddNode, 0.0, 0.0, |a, b| a + b);
binary_f64_node!(SubtractNode, 0.0, 0.0, |a, b| a - b);
binary_f64_node!(MultiplyNode, 0.0, 0.0, |a, b| a * b);
binary_f64_node!(DivideNode, 0.0, 1.0, |a, b| if b == 0.0 {
    f64::INFINITY
} else {
    a / b
});
binary_f64_node!(ModuloNode, 0.0, 1.0, |a, b| if b == 0.0 {
    f64::NAN
} else {
    a % b
});
binary_f64_node!(MinNode, 0.0, 0.0, |a: f64, b| a.min(b));
binary_f64_node!(MaxNode, 0.0, 0.0, |a: f64, b| a.max(b));

unary_f64_node!(NegateNode, |v: f64| -v);
unary_f64_node!(AbsNode, f64::abs);

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_core::*;

    fn exec_binary(node: &mut dyn ExecutableNode, a: f64, b: f64) -> f64 {
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(a));
        inputs.insert("b", Value::new(b));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        *ctx.take_outputs()["result"].downcast_ref::<f64>().unwrap()
    }

    fn exec_unary(node: &mut dyn ExecutableNode, v: f64) -> f64 {
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(v));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        *ctx.take_outputs()["result"].downcast_ref::<f64>().unwrap()
    }

    #[test]
    fn add() {
        assert_eq!(exec_binary(&mut AddNode::new(), 3.0, 4.0), 7.0);
    }

    #[test]
    fn subtract() {
        assert_eq!(exec_binary(&mut SubtractNode::new(), 10.0, 3.0), 7.0);
    }

    #[test]
    fn multiply() {
        assert_eq!(exec_binary(&mut MultiplyNode::new(), 3.0, 4.0), 12.0);
    }

    #[test]
    fn divide() {
        assert_eq!(exec_binary(&mut DivideNode::new(), 10.0, 4.0), 2.5);
    }

    #[test]
    fn divide_by_zero() {
        assert!(exec_binary(&mut DivideNode::new(), 10.0, 0.0).is_infinite());
    }

    #[test]
    fn modulo() {
        assert_eq!(exec_binary(&mut ModuloNode::new(), 10.0, 3.0), 1.0);
    }

    #[test]
    fn modulo_by_zero() {
        assert!(exec_binary(&mut ModuloNode::new(), 10.0, 0.0).is_nan());
    }

    #[test]
    fn min() {
        assert_eq!(exec_binary(&mut MinNode::new(), 3.0, 7.0), 3.0);
    }

    #[test]
    fn max() {
        assert_eq!(exec_binary(&mut MaxNode::new(), 3.0, 7.0), 7.0);
    }

    #[test]
    fn negate() {
        assert_eq!(exec_unary(&mut NegateNode::new(), 5.0), -5.0);
    }

    #[test]
    fn abs_neg() {
        assert_eq!(exec_unary(&mut AbsNode::new(), -7.0), 7.0);
    }

    #[test]
    fn abs_pos() {
        assert_eq!(exec_unary(&mut AbsNode::new(), 3.0), 3.0);
    }

    #[test]
    fn defaults_to_zero() {
        let mut node = AddNode::new();
        let inputs = InputSet::new();
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            *ctx.take_outputs()["result"].downcast_ref::<f64>().unwrap(),
            0.0
        );
    }

    #[test]
    fn divide_default_b_is_one() {
        let mut node = DivideNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(7.0f64));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            *ctx.take_outputs()["result"].downcast_ref::<f64>().unwrap(),
            7.0
        );
    }
}
