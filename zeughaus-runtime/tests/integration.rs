use zeughaus_core::{EdgeId, Value};
use zeughaus_transform::TransformPlugin;

mod common;

use common::GraphBuilder;

fn setup() -> GraphBuilder {
    let mut builder = GraphBuilder::new();
    builder.register_plugin(Box::new(TransformPlugin));
    builder
}

#[test]
fn add_pipeline_produces_correct_result() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64").unwrap();
    let const_b = builder.add_node("transform.const_f64").unwrap();
    let add = builder.add_node("transform.add").unwrap();
    let display = builder.add_node("transform.display").unwrap();

    builder.connect(const_a, "value", add, "a");
    builder.connect(const_b, "value", add, "b");
    let add_out_edge = builder.connect(add, "result", display, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(const_a, "value", Value::new(3.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(4.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    let result = executor.edge_value(add_out_edge).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&7.0));
}

/// Data flows from output to input, not the reverse: with edges reversed
/// internally the Add node would receive nothing and emit its default.
#[test]
fn edge_direction_from_output_to_input() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64").unwrap();
    let add = builder.add_node("transform.add").unwrap();
    let display = builder.add_node("transform.display").unwrap();

    // Const -> Add "a" (correct direction: from_node=const, to_node=add)
    builder.connect(const_a, "value", add, "a");
    let out_edge = builder.connect(add, "result", display, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(const_a, "value", Value::new(42.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // Add received 42.0 on pin "a", default 0.0 on pin "b" -> result = 42.0
    let result = executor.edge_value(out_edge).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&42.0));
}

/// A multi-hop chain: Const -> Negate -> Abs -> Display. Every intermediate
/// edge carries the value its own source produced.
#[test]
fn multi_hop_edge_direction() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64").unwrap();
    let neg = builder.add_node("transform.negate").unwrap();
    let abs = builder.add_node("transform.abs").unwrap();
    let display = builder.add_node("transform.display").unwrap();

    let e1 = builder.connect(src, "value", neg, "input");
    let e2 = builder.connect(neg, "result", abs, "input");
    let e3 = builder.connect(abs, "result", display, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(src, "value", Value::new(7.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // src=7 -> negate=-7 -> abs=7
    assert_eq!(
        executor.edge_value(e1).unwrap().downcast_ref::<f64>(),
        Some(&7.0)
    );
    assert_eq!(
        executor.edge_value(e2).unwrap().downcast_ref::<f64>(),
        Some(&-7.0)
    );
    assert_eq!(
        executor.edge_value(e3).unwrap().downcast_ref::<f64>(),
        Some(&7.0)
    );
}

#[test]
fn dirty_re_execution_after_parameter_change() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64").unwrap();
    let const_b = builder.add_node("transform.const_f64").unwrap();
    let add = builder.add_node("transform.add").unwrap();
    let display = builder.add_node("transform.display").unwrap();

    builder.connect(const_a, "value", add, "a");
    builder.connect(const_b, "value", add, "b");
    let out_edge = builder.connect(add, "result", display, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(const_a, "value", Value::new(10.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(20.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    assert_eq!(
        executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(),
        Some(&30.0)
    );

    // Change only const_b, re-execute dirty
    executor
        .set_parameter(const_b, "value", Value::new(100.0f64))
        .unwrap();
    executor.execute_dirty().unwrap();

    assert_eq!(
        executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(),
        Some(&110.0)
    );
}

/// One output feeding several inputs delivers the same value on every wire,
/// and each consumer is executed.
#[test]
fn fan_out_one_source_to_many_consumers() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64").unwrap();
    let add = builder.add_node("transform.add").unwrap();
    let mul = builder.add_node("transform.multiply").unwrap();
    let display_add = builder.add_node("transform.display").unwrap();
    let display_mul = builder.add_node("transform.display").unwrap();

    // src feeds both inputs of both consumers
    builder.connect(src, "value", add, "a");
    builder.connect(src, "value", add, "b");
    builder.connect(src, "value", mul, "a");
    builder.connect(src, "value", mul, "b");
    let add_out = builder.connect(add, "result", display_add, "input");
    let mul_out = builder.connect(mul, "result", display_mul, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(src, "value", Value::new(5.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // 5+5=10, 5*5=25
    assert_eq!(
        executor.edge_value(add_out).unwrap().downcast_ref::<f64>(),
        Some(&10.0)
    );
    assert_eq!(
        executor.edge_value(mul_out).unwrap().downcast_ref::<f64>(),
        Some(&25.0)
    );
}

/// A diamond (A -> B, A -> C, B -> D, C -> D): the node with two upstreams
/// runs once, after both of them.
#[test]
fn diamond_graph_correct_result() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64").unwrap();
    let add_left = builder.add_node("transform.add").unwrap();
    let mul_right = builder.add_node("transform.multiply").unwrap();
    let sub = builder.add_node("transform.subtract").unwrap();
    let display = builder.add_node("transform.display").unwrap();

    // src -> add_left "a" (b defaults to 0, so add_left = src + 0 = src)
    builder.connect(src, "value", add_left, "a");
    // src -> mul_right "a" and "b" (so mul_right = src * src)
    builder.connect(src, "value", mul_right, "a");
    builder.connect(src, "value", mul_right, "b");
    // add_left -> sub "a", mul_right -> sub "b" (sub = add_left - mul_right)
    builder.connect(add_left, "result", sub, "a");
    builder.connect(mul_right, "result", sub, "b");
    let out_edge = builder.connect(sub, "result", display, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(src, "value", Value::new(3.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // 3 - (3*3) = 3 - 9 = -6
    assert_eq!(
        executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(),
        Some(&-6.0)
    );
}

/// A branch decided by a value computed in the graph: the select node reads
/// its condition from an upstream comparison and follows it on a re-run.
#[test]
fn select_node_pipeline() {
    let mut builder = setup();
    let const_a = builder.add_node("transform.const_f64").unwrap();
    let const_b = builder.add_node("transform.const_f64").unwrap();
    let gt = builder.add_node("transform.greater_than").unwrap();
    let sel = builder.add_node("transform.select").unwrap();
    let display = builder.add_node("transform.display").unwrap();

    builder.connect(const_a, "value", gt, "a");
    builder.connect(const_b, "value", gt, "b");
    builder.connect(gt, "result", sel, "condition");
    builder.connect(const_a, "value", sel, "true_val");
    builder.connect(const_b, "value", sel, "false_val");
    let out_edge = builder.connect(sel, "result", display, "input");

    let mut executor = builder.build();
    executor
        .set_parameter(const_a, "value", Value::new(10.0f64))
        .unwrap();
    executor
        .set_parameter(const_b, "value", Value::new(5.0f64))
        .unwrap();
    executor.execute_all().unwrap();

    // 10 > 5 is true -> select true_val = 10
    assert_eq!(
        executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(),
        Some(&10.0)
    );

    // Flip: make a < b
    executor
        .set_parameter(const_a, "value", Value::new(2.0f64))
        .unwrap();
    executor.execute_dirty().unwrap();

    // 2 > 5 is false -> select false_val = 5
    assert_eq!(
        executor.edge_value(out_edge).unwrap().downcast_ref::<f64>(),
        Some(&5.0)
    );
}

/// Disconnecting an input leaves the node running on the default for that
/// pin, not on the value the wire last carried.
#[test]
fn workflow_disconnect_and_reexecute() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64").unwrap();
    let c2 = builder.add_node("transform.const_f64").unwrap();
    let add = builder.add_node("transform.add").unwrap();
    let disp = builder.add_node("transform.display").unwrap();

    builder.connect(c1, "value", add, "a");
    let edge_b = builder.connect(c2, "value", add, "b");
    let out = builder.connect(add, "result", disp, "input");

    let mut exec = builder.build();
    exec.set_parameter(c1, "value", Value::new(3.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(4.0f64)).unwrap();
    exec.execute_all().unwrap();

    assert_eq!(
        exec.edge_value(out).unwrap().downcast_ref::<f64>(),
        Some(&7.0)
    );

    exec.disconnect_edge(edge_b);
    exec.execute_dirty().unwrap();

    // Add now computes a=3 + b=0 (the default) = 3
    assert_eq!(
        exec.edge_value(out).unwrap().downcast_ref::<f64>(),
        Some(&3.0)
    );
}

/// A re-route: the new wire is seeded from its source's last output, so the
/// target recomputes from the new value without anything upstream re-running.
#[test]
fn workflow_reconnect_with_new_value() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64").unwrap();
    let c2 = builder.add_node("transform.const_f64").unwrap();
    let c3 = builder.add_node("transform.const_f64").unwrap();
    let add = builder.add_node("transform.add").unwrap();
    let disp = builder.add_node("transform.display").unwrap();

    builder.connect(c1, "value", add, "a");
    let edge_b = builder.connect(c2, "value", add, "b");
    let out = builder.connect(add, "result", disp, "input");

    let mut exec = builder.build();
    exec.set_parameter(c1, "value", Value::new(10.0f64))
        .unwrap();
    exec.set_parameter(c2, "value", Value::new(20.0f64))
        .unwrap();
    exec.set_parameter(c3, "value", Value::new(100.0f64))
        .unwrap();
    exec.execute_all().unwrap();

    assert_eq!(
        exec.edge_value(out).unwrap().downcast_ref::<f64>(),
        Some(&30.0)
    );

    // Disconnect c2, connect c3 instead
    exec.disconnect_edge(edge_b);
    exec.add_edge(EdgeId::next(), c3, "value".into(), add, "b".into());
    exec.execute_dirty().unwrap();

    // add = 10 + 100 = 110
    assert_eq!(
        exec.edge_value(out).unwrap().downcast_ref::<f64>(),
        Some(&110.0)
    );
}

/// Deleting a node mid-chain leaves a graph the next pass can still run: the
/// wires it carried are gone with it, and its former target reads its default.
#[test]
fn workflow_delete_node_mid_chain() {
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64").unwrap();
    let neg = builder.add_node("transform.negate").unwrap();
    let disp = builder.add_node("transform.display").unwrap();

    builder.connect(c1, "value", neg, "input");
    let out = builder.connect(neg, "result", disp, "input");

    let mut exec = builder.build();
    exec.set_parameter(c1, "value", Value::new(5.0f64)).unwrap();
    exec.execute_all().unwrap();

    exec.remove_node(neg);

    exec.mark_dirty(disp);
    exec.execute_dirty().unwrap();
    assert!(exec.edge_value(out).is_none());
}

/// A sink node (no outgoing edges) is executed and its incoming edge holds
/// the value it was handed -- the only place a sink's input is observable.
#[test]
fn sink_node_executes_and_incoming_edge_readable() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64").unwrap();
    let disp = builder.add_node("transform.display").unwrap();

    let edge_to_display = builder.connect(src, "value", disp, "input");

    let mut exec = builder.build();
    exec.set_parameter(src, "value", Value::new(42.0f64))
        .unwrap();
    exec.execute_all().unwrap();

    let val = exec.edge_value(edge_to_display).unwrap();
    assert_eq!(val.downcast_ref::<f64>(), Some(&42.0));
}
