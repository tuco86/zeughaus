use zeughaus_core::{DomainPlugin, Value};
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

// ===========================================================================
// Task 10: Graph save/load round-trip
// ===========================================================================

use zeughaus_core::{EdgeData, GraphDocument, NodeData};

/// Helper: convert a built executor into a GraphDocument for serialization.
fn executor_to_document(
    exec: &zeughaus_runtime::GraphExecutor,
    const_values: &[(zeughaus_core::NodeId, f64)],
) -> GraphDocument {
    let mut nodes = Vec::new();
    for nid in exec.graph.node_ids() {
        let gn = exec.graph.node(nid).unwrap();
        let params: Vec<(String, String)> = const_values
            .iter()
            .filter(|(id, _)| *id == nid)
            .map(|(_, v)| ("value".to_string(), v.to_string()))
            .collect();
        nodes.push(NodeData {
            id: gn.id.0,
            type_id: gn.type_id.clone(),
            display_name: gn.type_id.clone(),
            x: gn.position.0,
            y: gn.position.1,
            params,
        });
    }
    let mut edges = Vec::new();
    for edge in exec.graph.edges() {
        edges.push(EdgeData {
            id: edge.id.0,
            from_node: edge.from_node.0,
            from_pin: edge.from_pin.to_string(),
            to_node: edge.to_node.0,
            to_pin: edge.to_pin.to_string(),
        });
    }
    GraphDocument { nodes, edges }
}

/// Helper: rebuild executor from a GraphDocument.
fn document_to_executor(
    doc: &GraphDocument,
) -> zeughaus_runtime::GraphExecutor {
    use zeughaus_core::{EdgeId, EdgeSemantic, NodeConfig, NodeId};
    use zeughaus_runtime::{Graph, GraphEdge, GraphExecutor, GraphNode};

    let plugin = TransformPlugin;
    let mut graph = Graph::new();
    let mut execs: Vec<(NodeId, Box<dyn zeughaus_core::ExecutableNode>)> = Vec::new();

    for nd in &doc.nodes {
        let id = NodeId(nd.id);
        let mut exec = plugin.create_node(&nd.type_id).unwrap();
        let pin_defs = exec.pin_definitions().to_vec();

        // Apply saved parameters
        for (name, val_str) in &nd.params {
            if nd.type_id == "transform.const_f64" {
                if let Ok(f) = val_str.parse::<f64>() {
                    exec.set_parameter(name, Value::new(f)).unwrap();
                }
            }
        }

        graph.add_node(GraphNode {
            id,
            type_id: nd.type_id.clone(),
            config: NodeConfig::default(),
            pin_defs,
            position: (nd.x, nd.y),
        });
        execs.push((id, exec));
    }

    for ed in &doc.edges {
        graph.add_edge(GraphEdge {
            id: EdgeId(ed.id),
            from_node: NodeId(ed.from_node),
            from_pin: leak_pin(&ed.from_pin),
            to_node: NodeId(ed.to_node),
            to_pin: leak_pin(&ed.to_pin),
            semantic: EdgeSemantic::default(),
        });
    }

    let mut executor = GraphExecutor::new(graph);
    for (id, exec) in execs {
        executor.register_node(id, exec);
    }
    executor
}

/// Map common pin name strings to &'static str without leaking.
fn leak_pin(s: &str) -> &'static str {
    match s {
        "value" => "value",
        "result" => "result",
        "input" => "input",
        "a" => "a",
        "b" => "b",
        "total" => "result",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

/// Build graph: Const(3) + Const(4) -> Add -> Display
/// Serialize to JSON, deserialize, rebuild, execute, verify result = 7.
#[test]
fn save_load_round_trip_add_pipeline() {
    // Step 1: Build original graph
    let mut builder = setup();
    let c1 = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let c2 = builder.add_node("transform.const_f64", (100.0, 0.0)).unwrap();
    let add = builder.add_node("transform.add", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (400.0, 0.0)).unwrap();

    builder.connect(c1, "value", add, "a").unwrap();
    builder.connect(c2, "value", add, "b").unwrap();
    let out_edge = builder.connect(add, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(c1, "value", Value::new(3.0f64)).unwrap();
    exec.set_parameter(c2, "value", Value::new(4.0f64)).unwrap();
    exec.execute_all().unwrap();

    // Verify original works
    assert_eq!(exec.edge_value(out_edge).unwrap().downcast_ref::<f64>(), Some(&7.0));

    // Step 2: Convert to document and serialize
    let doc = executor_to_document(&exec, &[(c1, 3.0), (c2, 4.0)]);
    let json = serde_json::to_string_pretty(&doc).unwrap();

    // Step 3: Deserialize
    let loaded_doc: GraphDocument = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded_doc.nodes.len(), 4);
    assert_eq!(loaded_doc.edges.len(), 3);

    // Step 4: Rebuild executor from loaded document
    let mut exec2 = document_to_executor(&loaded_doc);
    exec2.execute_all().unwrap();

    // Step 5: Find the add->display edge and verify result
    let add_disp_edge = loaded_doc.edges.iter().find(|e| e.from_pin == "result" && e.to_pin == "input").unwrap();
    let result = exec2.edge_value(zeughaus_core::EdgeId(add_disp_edge.id)).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&7.0));
}

/// Round-trip with a longer chain: Const -> Negate -> Abs -> Display = positive value.
#[test]
fn save_load_round_trip_chain() {
    let mut builder = setup();
    let src = builder.add_node("transform.const_f64", (0.0, 0.0)).unwrap();
    let neg = builder.add_node("transform.negate", (100.0, 0.0)).unwrap();
    let abs = builder.add_node("transform.abs", (200.0, 0.0)).unwrap();
    let disp = builder.add_node("transform.display", (300.0, 0.0)).unwrap();

    builder.connect(src, "value", neg, "input").unwrap();
    builder.connect(neg, "result", abs, "input").unwrap();
    let out = builder.connect(abs, "result", disp, "input").unwrap();

    let mut exec = builder.build().unwrap();
    exec.set_parameter(src, "value", Value::new(7.0f64)).unwrap();
    exec.execute_all().unwrap();
    assert_eq!(exec.edge_value(out).unwrap().downcast_ref::<f64>(), Some(&7.0));

    // Round-trip
    let doc = executor_to_document(&exec, &[(src, 7.0)]);
    let json = serde_json::to_string(&doc).unwrap();
    let loaded: GraphDocument = serde_json::from_str(&json).unwrap();
    let mut exec2 = document_to_executor(&loaded);
    exec2.execute_all().unwrap();

    // Find the abs->display edge by locating the display node
    let disp_id = loaded.nodes.iter().find(|n| n.type_id == "transform.display").unwrap().id;
    let abs_disp_edge = loaded.edges.iter().find(|e| e.to_node == disp_id).unwrap();
    let result = exec2.edge_value(zeughaus_core::EdgeId(abs_disp_edge.id)).unwrap();
    assert_eq!(result.downcast_ref::<f64>(), Some(&7.0));
}
