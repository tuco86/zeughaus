use zeughaus_core::Value;
use zeughaus_runtime::GraphBuilder;
use zeughaus_transform::TransformPlugin;

fn setup() -> GraphBuilder {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));
    builder
}

// -- Basic pipeline tests --

#[test]
fn add_pipeline_produces_correct_result() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(const_a, "value", add, "a").unwrap();
    builder.connect(const_b, "value", add, "b").unwrap();
    let add_out_edge = builder.connect(add, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(const_a, "value", Value::new(3.0f64)).unwrap();
    executor.set_parameter(const_b, "value", Value::new(4.0f64)).unwrap();
    executor.execute_all().unwrap();

    let result = executor.edge_value(add_out_edge).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&7.0));
}

#[test]
fn multiply_pipeline() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let mul = builder.add_node("transform.multiply", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(const_a, "value", mul, "a").unwrap();
    builder.connect(const_b, "value", mul, "b").unwrap();
    let mul_out = builder.connect(mul, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(const_a, "value", Value::new(5.0f64)).unwrap();
    executor.set_parameter(const_b, "value", Value::new(6.0f64)).unwrap();
    executor.execute_all().unwrap();

    let result = executor.edge_value(mul_out).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&30.0));
}

#[test]
fn subtract_pipeline() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let sub = builder.add_node("transform.subtract", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(const_a, "value", sub, "a").unwrap();
    builder.connect(const_b, "value", sub, "b").unwrap();
    let sub_out = builder.connect(sub, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(const_a, "value", Value::new(10.0f64)).unwrap();
    executor.set_parameter(const_b, "value", Value::new(3.0f64)).unwrap();
    executor.execute_all().unwrap();

    let result = executor.edge_value(sub_out).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&7.0));
}

// -- Edge direction tests (the "gradient" test the user asked about) --

/// Verify that data flows from output to input, not the reverse.
/// If edges were reversed internally, the Add node would receive nothing
/// and output 0 instead of the expected result.
#[test]
fn edge_direction_from_output_to_input() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    // Const -> Add "a" (correct direction: from_node=const, to_node=add)
    builder.connect(const_a, "value", add, "a").unwrap();
    let out_edge = builder.connect(add, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(const_a, "value", Value::new(42.0f64)).unwrap();
    executor.execute_all().unwrap();

    // Add received 42.0 on pin "a", default 0.0 on pin "b" -> result = 42.0
    let result = executor.edge_value(out_edge).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&42.0));
}

/// Verify a multi-hop chain: Const -> Negate -> Abs -> Display
/// Tests that intermediate edges carry correct values in correct direction.
#[test]
fn multi_hop_edge_direction() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let neg = builder.add_node("transform.negate", (100.0, 0.0)).unwrap();
    let abs = builder.add_node("transform.abs", (200.0, 0.0)).unwrap();
    let display = builder.add_node("transform.display", (300.0, 0.0)).unwrap();

    let e1 = builder.connect(src, "value", neg, "input").unwrap();
    let e2 = builder.connect(neg, "result", abs, "input").unwrap();
    let e3 = builder.connect(abs, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(src, "value", Value::new(7.0f64)).unwrap();
    executor.execute_all().unwrap();

    // src=7 -> negate=-7 -> abs=7
    assert_eq!(executor.edge_value(e1).unwrap().downcast_ref::<f64>(), Some(&7.0));
    assert_eq!(executor.edge_value(e2).unwrap().downcast_ref::<f64>(), Some(&-7.0));
    assert_eq!(executor.edge_value(e3).unwrap().downcast_ref::<f64>(), Some(&7.0));
}

// -- Dirty propagation tests --

#[test]
fn dirty_re_execution_after_parameter_change() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(const_a, "value", add, "a").unwrap();
    builder.connect(const_b, "value", add, "b").unwrap();
    let out_edge = builder.connect(add, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(const_a, "value", Value::new(10.0f64)).unwrap();
    executor.set_parameter(const_b, "value", Value::new(20.0f64)).unwrap();
    executor.execute_all().unwrap();

    assert_eq!(executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&30.0));

    // Change only const_b, re-execute dirty
    executor.set_parameter(const_b, "value", Value::new(100.0f64)).unwrap();
    executor.execute_dirty().unwrap();

    assert_eq!(executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&110.0));
}

// -- Fan-out: one output feeding multiple inputs --

#[test]
fn fan_out_one_source_to_many_consumers() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let mul = builder.add_node("transform.multiply", (200.0, 100.0)).unwrap();
    let display_add = builder.add_node("transform.display", (400.0, 0.0)).unwrap();
    let display_mul = builder.add_node("transform.display", (400.0, 100.0)).unwrap();

    // src feeds both "a" pins
    builder.connect(src, "value", add, "a").unwrap();
    builder.connect(src, "value", add, "b").unwrap();
    builder.connect(src, "value", mul, "a").unwrap();
    builder.connect(src, "value", mul, "b").unwrap();
    let add_out = builder.connect(add, "result", display_add, "input").unwrap();
    let mul_out = builder.connect(mul, "result", display_mul, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(src, "value", Value::new(5.0f64)).unwrap();
    executor.execute_all().unwrap();

    // 5+5=10, 5*5=25
    assert_eq!(executor.edge_value(add_out).unwrap().downcast_ref::<f64>(), Some(&10.0));
    assert_eq!(executor.edge_value(mul_out).unwrap().downcast_ref::<f64>(), Some(&25.0));
}

// -- Diamond graph: A -> B, A -> C, B -> D, C -> D --

#[test]
fn diamond_graph_correct_result() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let add_left = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let mul_right = builder.add_node("transform.multiply", (200.0, 100.0)).unwrap();
    let sub = builder.add_node("transform.subtract", (400.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (600.0, 50.0)).unwrap();

    // src -> add_left "a" (b defaults to 0, so add_left = src + 0 = src)
    builder.connect(src, "value", add_left, "a").unwrap();
    // src -> mul_right "a" and "b" (so mul_right = src * src)
    builder.connect(src, "value", mul_right, "a").unwrap();
    builder.connect(src, "value", mul_right, "b").unwrap();
    // add_left -> sub "a", mul_right -> sub "b" (sub = add_left - mul_right = src - src^2)
    builder.connect(add_left, "result", sub, "a").unwrap();
    builder.connect(mul_right, "result", sub, "b").unwrap();
    let out_edge = builder.connect(sub, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(src, "value", Value::new(3.0f64)).unwrap();
    executor.execute_all().unwrap();

    // 3 - (3*3) = 3 - 9 = -6
    assert_eq!(executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&-6.0));
}

// -- Select (conditional) pipeline --

#[test]
fn select_node_pipeline() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let const_b = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let gt = builder.add_node("transform.greater_than", (200.0, 0.0)).unwrap();
    let sel = builder.add_node("transform.select", (400.0, 0.0)).unwrap();
    let display = builder.add_node("transform.display", (600.0, 0.0)).unwrap();

    builder.connect(const_a, "value", gt, "a").unwrap();
    builder.connect(const_b, "value", gt, "b").unwrap();
    builder.connect(gt, "result", sel, "condition").unwrap();
    builder.connect(const_a, "value", sel, "true_val").unwrap();
    builder.connect(const_b, "value", sel, "false_val").unwrap();
    let out_edge = builder.connect(sel, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(const_a, "value", Value::new(10.0f64)).unwrap();
    executor.set_parameter(const_b, "value", Value::new(5.0f64)).unwrap();
    executor.execute_all().unwrap();

    // 10 > 5 is true -> select true_val = 10
    assert_eq!(executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&10.0));

    // Flip: make a < b
    executor.set_parameter(const_a, "value", Value::new(2.0f64)).unwrap();
    executor.execute_dirty().unwrap();

    // 2 > 5 is false -> select false_val = 5
    assert_eq!(executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&5.0));
}

// -- Error handling --

#[test]
fn unknown_node_type_returns_error() {
    let mut builder = setup();
    assert!(builder.add_node("nonexistent.type", (0.0, 0.0)).is_err());
}

// -- Clamp pipeline --

#[test]
fn clamp_pipeline() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let min = builder.add_node("transform.const_f64", (0.0, 50.0)).unwrap();
    let max = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let clamp = builder.add_node("transform.clamp", (200.0, 50.0)).unwrap();
    let display = builder.add_node("transform.display", (400.0, 50.0)).unwrap();

    builder.connect(src, "value", clamp, "input").unwrap();
    builder.connect(min, "value", clamp, "min").unwrap();
    builder.connect(max, "value", clamp, "max").unwrap();
    let out_edge = builder.connect(clamp, "result", display, "input").unwrap();

    let mut executor = builder.build().unwrap();
    executor.set_parameter(src, "value", Value::new(15.0f64)).unwrap();
    executor.set_parameter(min, "value", Value::new(0.0f64)).unwrap();
    executor.set_parameter(max, "value", Value::new(10.0f64)).unwrap();
    executor.execute_all().unwrap();

    // 15 clamped to [0, 10] = 10
    assert_eq!(executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&10.0));
}

// ===========================================================================
// Task 01: Real editor workflow simulation tests
// ===========================================================================

/// Full workflow: spawn nodes, connect, set values, verify result.
#[test]
fn workflow_const_add_display() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let c2 = builder.add_node("transform.const_f64", (100.0, 0.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    builder.connect(c1, "value", add, "a").unwrap();
    builder.connect(c2, "value", add, "b").unwrap();
    let out = builder.connect(add, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(3.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(4.0f64)).unwrap();
    exec.execute_all().unwrap();

    assert_eq!(exec.edge_value(out).unwrap().downcast_ref::<f64>(), Some(&7.0));
}

/// Disconnect one input from Add, re-execute.
/// Add should get default 0.0 for the missing input.
#[test]
fn workflow_disconnect_and_reexecute() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let c2 = builder.add_node("transform.const_f64", (100.0, 0.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    builder.connect(c1, "value", add, "a").unwrap();
    let edge_b = builder.connect(c2, "value", add, "b").unwrap();
    let out = builder.connect(add, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(3.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(4.0f64)).unwrap();
    exec.execute_all().unwrap();

    assert_eq!(exec.edge_value(out).unwrap().downcast_ref::<f64>(), Some(&7.0));

    // Disconnect c2 from add's "b" pin (properly clears cache)
    exec.disconnect_edge(edge_b);
    exec.execute_dirty().unwrap();

    // Add should now compute a=3 + b=0(default) = 3
    assert_eq!(exec.edge_value(out).unwrap().downcast_ref::<f64>(), Some(&3.0));
}

/// Reconnect a different value after disconnect.
#[test]
fn workflow_reconnect_with_new_value() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let c2 = builder.add_node("transform.const_f64", (100.0, 0.0)).unwrap();
    let c3 = builder.add_node("transform.const_f64", (100.0, 100.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    builder.connect(c1, "value", add, "a").unwrap();
    let edge_b = builder.connect(c2, "value", add, "b").unwrap();
    let out = builder.connect(add, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(10.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(20.0f64)).unwrap();
    exec.set_parameter(c3, "value", Value::new(100.0f64)).unwrap();
    exec.execute_all().unwrap();

    assert_eq!(exec.edge_value(out).unwrap().downcast_ref::<f64>(), Some(&30.0));

    // Disconnect c2, connect c3 instead
    exec.disconnect_edge(edge_b);
    use zeughaus_core::{EdgeId, EdgeSemantic};
    use zeughaus_runtime::GraphEdge;
    let new_edge = EdgeId::next();
    exec.graph.add_edge(GraphEdge {
        id: new_edge,
        from_node: c3,
        from_pin: "value",
        to_node: add,
        to_pin: "b",
        semantic: EdgeSemantic::default(),
    });
    exec.mark_dirty_downstream(c3);
    exec.execute_dirty().unwrap();

    // add = 10 + 100 = 110
    assert_eq!(exec.edge_value(out).unwrap().downcast_ref::<f64>(), Some(&110.0));
}

/// Delete a node mid-chain. Remaining graph should not panic.
#[test]
fn workflow_delete_node_mid_chain() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let neg = builder.add_node("transform.negate", (100.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (300.0, 0.0)).unwrap();

    builder.connect(c1, "value", neg, "input").unwrap();
    builder.connect(neg, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(5.0f64)).unwrap();
    exec.execute_all().unwrap();

    // Delete negate node (middle of chain) -- properly cleans up edges + cache
    exec.remove_node(neg);

    // Executing should not panic -- negate is gone, display has no input
    exec.mark_dirty(disp);
    exec.execute_dirty().unwrap();
    // No assertion on value -- just verify no panic
}

/// Verify that a graph with only const nodes (no connections) executes without error.
#[test]
fn workflow_unconnected_nodes() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let c2 = builder.add_node("transform.const_f64", (100.0, 0.0)).unwrap();
    let _disp = builder.add_node("transform.display", (200.0, 0.0)).unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(1.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(2.0f64)).unwrap();
    exec.execute_all().unwrap();
    // No panic, no error
}

// ===========================================================================
// Task 03: Sink node execution and incoming edge readability
// ===========================================================================

/// Display is a sink node (no outgoing edges). It must execute and its
/// incoming edge cache must be readable for the editor to show values.
#[test]
fn sink_node_executes_and_incoming_edge_readable() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (200.0, 0.0)).unwrap();

    let edge_to_display = builder.connect(src, "value", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(src, "value", Value::new(42.0f64)).unwrap();
    exec.execute_all().unwrap();

    // The incoming edge to display must have the cached value from src
    let val = exec.edge_value(edge_to_display).unwrap();
    assert_eq!(val.downcast_ref::<f64>(), Some(&42.0));
}

/// Multiple sink nodes fed by the same source via fan-out.
#[test]
fn multiple_sink_nodes_from_same_source() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let disp1 = builder.add_node("transform.display", (200.0, 0.0)).unwrap();
    let disp2 = builder.add_node("transform.display", (200.0, 100.0)).unwrap();

    let e1 = builder.connect(src, "value", disp1, "input").unwrap();
    let e2 = builder.connect(src, "value", disp2, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(src, "value", Value::new(99.0f64)).unwrap();
    exec.execute_all().unwrap();

    assert_eq!(exec.edge_value(e1).unwrap().downcast_ref::<f64>(), Some(&99.0));
    assert_eq!(exec.edge_value(e2).unwrap().downcast_ref::<f64>(), Some(&99.0));
}

/// Sink node after a chain: Const -> Add -> Display.
/// Display's incoming edge must carry the Add result.
#[test]
fn sink_node_after_chain() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let c2 = builder.add_node("transform.const_f64", (0.0, 100.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    builder.connect(c1, "value", add, "a").unwrap();
    builder.connect(c2, "value", add, "b").unwrap();
    let edge_to_disp = builder.connect(add, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(5.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(3.0f64)).unwrap();
    exec.execute_all().unwrap();

    assert_eq!(exec.edge_value(edge_to_disp).unwrap().downcast_ref::<f64>(), Some(&8.0));

    // Update a source and dirty-execute: display's incoming edge should update
    exec.set_parameter(c1, "value", Value::new(10.0f64)).unwrap();
    exec.execute_dirty().unwrap();

    assert_eq!(exec.edge_value(edge_to_disp).unwrap().downcast_ref::<f64>(), Some(&13.0));
}

/// Accumulator is a stateful sink-like node. Verify it accumulates across executions.
#[test]
fn accumulator_sink_node() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let acc = builder.add_node("transform.accumulator", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    builder.connect(src, "value", acc, "input").unwrap();
    let edge_to_disp = builder.connect(acc, "total", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(src, "value", Value::new(5.0f64)).unwrap();
    exec.execute_all().unwrap();

    // First execution: accumulator total = 0 + 5 = 5
    assert_eq!(exec.edge_value(edge_to_disp).unwrap().downcast_ref::<f64>(), Some(&5.0));

    // Execute again (re-mark dirty): total = 5 + 5 = 10
    exec.mark_dirty_downstream(src);
    exec.execute_dirty().unwrap();

    assert_eq!(exec.edge_value(edge_to_disp).unwrap().downcast_ref::<f64>(), Some(&10.0));
}
