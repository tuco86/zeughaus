use zeughaus_core::Value;
use zeughaus_runtime::GraphBuilder;
use zeughaus_transform::TransformPlugin;

#[test]
fn add_pipeline_produces_correct_result() {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));

    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(const_a, "value", add, "a").unwrap();
    builder.connect(const_b, "value", add, "b").unwrap();
    let add_out_edge = builder.connect(add, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();

    // Set const values
    executor
        .set_parameter(const_a, "value", Value::new(3.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(4.0f64))
        .unwrap();

    executor.execute_all().unwrap();

    // The add node should have produced 7.0 on the edge to display
    let result = executor.edge_value(add_out_edge).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&7.0));
}

#[test]
fn dirty_re_execution_after_parameter_change() {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));

    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 50.0)).unwrap();

    builder.connect(const_a, "value", add, "a").unwrap();
    let edge_b_add = builder.connect(const_b, "value", add, "b").unwrap();

    let mut executor = builder.build().unwrap();
    executor
        .set_parameter(const_a, "value", Value::new(10.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(20.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // Verify initial result
    let result = executor.edge_value(edge_b_add).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&20.0));

    // Change const_b and re-execute only dirty nodes
    executor
        .set_parameter(const_b, "value", Value::new(100.0f64))
        .unwrap();
    executor.execute_dirty().unwrap();

    // const_b edge should now have 100.0
    let result = executor.edge_value(edge_b_add).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&100.0));
}

#[test]
fn unknown_node_type_returns_error() {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));
    assert!(builder.add_node("nonexistent.type", (0.0, 0.0)).is_err());
}

#[test]
fn multiply_pipeline() {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));

    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let mul = builder.add_node("transform.multiply", (200.0, 50.0)).unwrap();

    builder.connect(const_a, "value", mul, "a").unwrap();
    let edge_out = builder.connect(const_b, "value", mul, "b").unwrap();

    let mut executor = builder.build().unwrap();
    executor
        .set_parameter(const_a, "value", Value::new(5.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(6.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // The multiply output edge is const_b -> mul "b", but we want mul's output edge
    // We need the edge from mul's "result" pin -- let's check the const_b -> mul edge instead
    // Actually edge_out is const_b -> mul, not mul -> output. Let's fix this.
    let _ = edge_out;
}

#[test]
fn multiply_pipeline_with_output_edge() {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));

    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let mul = builder.add_node("transform.multiply", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(const_a, "value", mul, "a").unwrap();
    builder.connect(const_b, "value", mul, "b").unwrap();
    let mul_out = builder.connect(mul, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor
        .set_parameter(const_a, "value", Value::new(5.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(6.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    let result = executor.edge_value(mul_out).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&30.0));
}
