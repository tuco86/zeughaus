# Zeughaus Autonomous Work Queue

Each task is self-contained. The agent picks the first `[ ]` task, completes it,
commits, marks it `[x]`. Every 4th task is a review/simplify cycle.

## Rules for the Agent

1. Read CLAUDE.md for project context.
2. Find the first `[ ]` task below. Do ONLY that task.
3. Run `cargo test` and `cargo clippy -- -D warnings` before committing.
4. If tests fail, fix them. Do not commit broken code.
5. Mark the task `[x]` and commit TASKS.md together with code changes.
6. One task = one commit. Do not do multiple tasks.
7. If a task is unclear, do your best interpretation and document what you did.

## Queue

### 01 - Integration tests for real workflows
Write tests in `zeughaus-runtime/tests/integration.rs` that cover:
- Spawn Const(3) + Const(4) -> Add -> Display, verify result is 7
- Disconnect one const from Add, re-execute, verify Add outputs 3 (a=3, b=default 0)
- Reconnect with different value, verify update propagates
- Delete a node mid-chain, verify no panic and remaining graph still works
- [x] DONE - Added 5 workflow tests: spawn+connect+verify, disconnect+reexecute, reconnect, delete mid-chain, unconnected nodes

### 02 - Fix stale cache after edge disconnect
When an edge is removed, the downstream node still sees the old cached value.
The executor must clear the edge cache entry on disconnect and re-execute
downstream with missing inputs defaulting to their pin defaults.
- [x] DONE - Added disconnect_edge() and remove_node() to GraphExecutor that clear cache. Updated editor and tests to use them.

### 03 - Fix execution for sink nodes
Nodes with no outgoing edges (Display, Accumulator) must still execute and
their incoming edge caches must be readable. Verify with a test.
- [x] DONE - Verified sink nodes execute correctly. Added 4 tests: single sink, multiple sinks from fan-out, sink after chain with dirty update, accumulator stateful sink.

### 04 - REVIEW: simplify and remove dead code
Look at every file. Remove unused imports, dead code, unnecessary abstractions.
Run `cargo clippy -- -D warnings`. Check if any node types are redundant or
could be combined. Apply "nothing left to remove" principle.
- [x] DONE - Removed unused Event/EventMeta module, removed dead node_mut API, removed unused macro param. Added doc comments to reserved-but-unimplemented design-doc types (DataMode::Stream, EdgeSemantic::Queue, PinKind, NodeConfig::capture).

### 05 - Stable topological sort
Topo sort must be deterministic. Add NodeId-based tiebreaking to Kahn's algorithm.
Add test: two independent nodes, verify order is always sorted by ID.
- [x] DONE - Replaced VecDeque with BinaryHeap<Reverse<NodeId>> for deterministic tiebreaking. Added tests: independent nodes always sorted by ID, diamond tiebreak consistent over 20 runs.

### 06 - Display trait for Value
Implement `fmt::Display` for Value. f64 formats as number, String as-is, bool
as true/false. Remove all `format_value()` helper functions and use Display.
- [x] DONE - Implemented fmt::Display for Value (f64, String, bool, i64, i32, u64). Removed format_value() from app.rs, replaced inline formatting in display.rs and to_string.rs with val.to_string(). Added 4 Display tests.

### 07 - Macro for binary and unary f64 nodes
Create macros `binary_f64_node!` and `unary_f64_node!` (trig.rs already has one).
Refactor Add, Sub, Mul, Div, Min, Max, Mod, Pow to use the binary macro.
Refactor Negate, Abs to use the unary macro. Delete the old verbose files.
- [x] DONE - Created binary_f64_node! and unary_f64_node! macros in macros.rs. Consolidated Add, Sub, Mul, Div, Mod, Min, Max into math.rs (binary macro). Moved Negate, Abs into math.rs (unary macro). Updated trig.rs to use shared macro. Deleted 8 files, net -591 lines.

### 08 - REVIEW: test coverage audit
Run `cargo test` and check which modules have zero tests. Write at least one
meaningful test for every module that lacks coverage. Focus on edge cases.
- [x] DONE - Added 5 tests to cache.rs (get/set/overwrite/remove/noop) and 5 tests to builder.rs (unique IDs, unknown type error, empty build, cycle detection, build+execute). All previously untested runtime modules now covered.

### 09 - Serde for core types
Add `#[derive(Serialize, Deserialize)]` to NodeId, EdgeId, PinId, PinDefinition,
EdgeSemantic, NodeConfig, NodeDefinition. These are prerequisites for SpacetimeDB.
- [x] DONE - Added Serialize/Deserialize to NodeId, EdgeId, PinId (transparent as u64), DataMode, PinKind, PinDirection, EdgeSemantic, NodeConfig. Added serde round-trip test. NodeDefinition/PinDefinition skipped (contain &'static str, handled by GraphDocument).

### 10 - Graph save/load round-trip test
Create a graph in code, serialize to GraphDocument JSON, deserialize back,
re-create the graph+executor from it, execute, verify identical results.
This tests the entire save/load pipeline without the editor.
- [ ] DO THIS TASK

### 11 - Fix widget tree panic
The "Downcast widget state" crash in iced. Investigate root cause by reading
iced_core source. Likely fix: ensure node_order is stable, use consistent
widget structure (same number of children per node regardless of state).
- [ ] DO THIS TASK

### 12 - REVIEW: API consistency
Review all plugin traits and node APIs. Are pin names consistent? Do all nodes
follow the same execute/emit/flush pattern? Is there unnecessary duplication
between plugins? Document any inconsistencies and fix them.
- [ ] DO THIS TASK

### 13 - Command palette categories
Group nodes in the palette by category with visual headers.
Math nodes together, Trig together, Logic together, etc.
- [ ] DO THIS TASK

### 14 - Status bar
Add a bar at the bottom of the editor showing: node count, edge count,
last execution duration, error message if any node failed.
- [ ] DO THIS TASK

### 15 - Pin type validation at runtime
Add type checking in the executor: when building InputSet, verify the cached
Value's type matches the target pin's type_name. Log a warning on mismatch.
- [ ] DO THIS TASK

### 16 - REVIEW: remove unnecessary complexity
Look at the entire codebase with fresh eyes. What can be removed? What nodes
are never useful? What abstractions are premature? Simplify aggressively.
Apply Steve Jobs principle: done when nothing left to remove.
- [ ] DO THIS TASK
