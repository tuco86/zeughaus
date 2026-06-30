# Zeughaus - Design Document

A visual dataflow workbench built on [iced](https://github.com/iced-rs/iced) and [iced_nodegraph](https://github.com/your/iced_nodegraph).
Zeughaus connects heterogeneous tools -- DLL injection, database management, AI pipelines, process instrumentation -- through a unified node graph interface.

**Status**: Pre-development design phase

## Vision

Zeughaus is a Swiss-army-knife node editor where pluggable domains (DLL injection, database schemas, AI inference, workflow automation) are composed visually. Each domain lives in its own subgraph with domain-specific semantics, while a shared dataflow model connects them.

The node graph is the universal interface. The execution happens elsewhere.

## Architecture Overview

```
+----------------------------------+
|  Editor UI (iced / WASM)         |  Graph editing, visualization, previews
+----------------------------------+
|  Document Layer (SpacetimeDB)    |  Graph state, collaboration, versioning
+----------------------------------+
|  Execution Layer                 |  Runs nodes
|  +-- Local Runner (native)       |    DLL inject, filesystem, GPU
|  +-- Remote Runner (agent/VM)    |    Kubernetes, SSH, managed
|  +-- Browser Runner (WASM)       |    Transforms, previews, lightweight
+----------------------------------+
|  Storage Layer                   |  Persistence
|  +-- Postgres                    |    Production data, captured events
|  +-- SQLite                      |    Local/offline
|  +-- SpacetimeDB                 |    Collaborative graph state
+----------------------------------+
```

### Separation of Editor and Executor

The editor designs the graph and visualizes results. Execution is delegated to runners.
This separation resolves the tension between WASM browser support and native-only capabilities (DLL injection, GPU access).

A browser-based editor can submit graphs to a native local runner or a remote VM agent.

## Collaboration (SpacetimeDB)

Real-time collaborative graph editing via [SpacetimeDB](https://spacetimedb.com/).

- Graph definition (nodes, edges, positions, config) synced as shared state
- Server-authoritative conflict resolution
- Execution state optionally visible to collaborators (read-only)
- Execution control remains per-user / per-runner

**WASM Compatibility**: SpacetimeDB Rust SDK WASM support is actively developed in
[PR #4183](https://github.com/clockworklabs/SpacetimeDB/pull/4183) (staff-driven, `web` feature flag).
No architectural compromise needed -- SpacetimeDB and WASM browser clients will coexist.

### Staging Model

Graph changes follow a git-like deployment model:

| Stage | Purpose |
|-------|---------|
| **Draft** | Live collaborative editing, changes are immediate |
| **Staged** | Explicit "ready for review" checkpoint |
| **Deployed** | The version the runner executes |

This prevents half-finished edits from breaking running flows.
Rollback to any previously deployed version is always possible.

## Domain Subgraphs

Different domains live in separate subgraphs. Each subgraph is internally consistent
(own type system, execution model) but exposes a uniform interface to the parent graph.

```
[Orchestration Graph]
  +-- [DB Schema Subgraph]         -> generates SQL
  +-- [Memory Analyzer Subgraph]   -> reads process data
  +-- [Transform Subgraph]         -> processes results
  +-- [AI Pipeline Subgraph]       -> GPU inference
```

A subgraph appears as a single node in its parent graph with explicitly exposed input/output pins.
Double-click navigates into the subgraph.

### Planned Domains

| Domain | Node Types | Execution Model |
|--------|-----------|-----------------|
| Process / DLL Injection | Processes, Inject, Memory Read/Write, Hook | Imperative, event-driven |
| Database (FileMaker-style) | Connect, Table, Query, Insert, Schema Viz | Declarative, generates SQL |
| AI / GPU | ONNX Inference, Preprocessing, Postprocessing | Pipeline, batch or stream |
| Workflow / Automation | HTTP, Transform, Filter, Schedule | Sequential, event-triggered |
| Screen Capture / Video | Capture, Encode, Stream, Overlay | Real-time stream |

### Machine Learning (Keras) -- implemented

The `zeughaus-ml` plugin turns Keras (TensorFlow) layers into nodes so a neural
network can be designed entirely in the graph and exported as a runnable
`keras.Sequential` program. A `KerasModel` value (an ordered layer stack plus an
optional compile config) flows through the chain: each layer node consumes a
model and emits it extended by one layer, mirroring the LLM plugin's
Conversation pattern.

Layer nodes are data-driven: every supported layer is a row in a static `LAYERS`
table (Input, Dense, Conv1D/2D, Max/Average/GlobalAveragePooling, Flatten,
Reshape, Dropout, BatchNormalization, LayerNormalization, Activation, LSTM, GRU,
Embedding). Each parameter is typed -- strings are quoted in codegen, raw
literals (numbers, tuples, bools) are emitted verbatim, and a blank value omits
the kwarg so Keras applies its own default. The Export node validates at the
boundary (non-empty model, Input layer first) and renders the final Python.

Example graph:

```
[Input (28,28,1)] -> [Conv2D 32 (3,3) relu] -> [MaxPooling2D (2,2)]
  -> [Flatten] -> [Dropout 0.5] -> [Dense 10 softmax]
  -> [Compile adam/categorical_crossentropy/accuracy] -> [Export Code]
```

The Export node emits:

```python
import keras
from keras import layers

model = keras.Sequential([
    layers.Input(shape=(28, 28, 1)),
    layers.Conv2D(filters=32, kernel_size=(3, 3), activation='relu'),
    layers.MaxPooling2D(pool_size=(2, 2)),
    layers.Flatten(),
    layers.Dropout(rate=0.5),
    layers.Dense(units=10, activation='softmax'),
])
model.compile(optimizer='adam', loss='categorical_crossentropy', metrics=['accuracy'])
model.summary()
```

The plugin is pure codegen with no platform dependencies, so it is available in
the wasm editor as well; native runners execute the exported code.

## Dataflow Model

### Push/Pull Reactive

Every connection follows push/pull semantics:

- **Push**: Source writes a new value, notifies downstream consumers
- **Pull**: Consumer requests the current value. Triggers first computation if no cached value exists (lazy initialization)
- **Cache**: Every edge holds the last value. Always readable via pull

```
[File Node]
  Never executed, cache empty
       |
       v (Pull)
  Consumer requests value -> File Node reads file -> cache filled -> value delivered

  File changes externally -> File Node pushes update -> cache updated -> consumers notified
```

### Execution Semantics

Runtime uses dirty-flag propagation:

1. Source changes -> mark all downstream nodes as dirty
2. Only dirty nodes are recomputed
3. Recomputation happens on pull (lazy) or immediately for trigger-connected consumers (eager)

Optimal for small, incremental changes -- only affected nodes recompute.

### Pin Kinds

**Output pins** declare what they produce:

| DataMode | Description | Example |
|----------|-------------|---------|
| `Stream` | Continuous data, high frequency | Frame data, video, events per tick |
| `Value` | Single value, changes occasionally | Config, PID, connection string |

**Input pins** declare how they consume:

| PinKind | Description | Example |
|---------|-------------|---------|
| `Trigger` | New data causes node execution | Frame input on a parser node |
| `Sample` | Holds latest value, read passively on trigger | Config offsets on a parser node |

Any output mode is compatible with any input kind:
- `Value` output -> `Trigger` input: fires on every value change
- `Stream` output -> `Sample` input: holds only the latest frame

Node authors set defaults. Users can toggle trigger/sample per pin in the editor.

### Atomic Output (Multi-Pin Synchronization)

Nodes with multiple logically related outputs use atomic flush:

```rust
fn execute(&mut self, ctx: &mut NodeContext) {
    let data = self.read_memory();
    let frame = self.capture_frame();

    ctx.emit("unit_data", data);   // buffered
    ctx.emit("video", frame);      // buffered
    ctx.flush();                   // now downstream triggers
}
```

Between `emit` and `flush`, outputs are buffered. `flush` releases them atomically,
ensuring consumers always see consistent pairs.

For synchronizing outputs from **different** nodes, an explicit `Zip` node waits for
one value from each input before emitting a tuple.

### Edge Transport Abstraction

Edges in the graph are abstract connections. Transport depends on node placement:

| Placement | Transport |
|-----------|-----------|
| Same process | `tokio::mpsc` channel (zero-copy) |
| Same machine | Named pipe / shared memory |
| Different machines | gRPC stream / WebSocket |

The graph designer always sees the same edge. The runtime resolves transport based on placement.

### Edge Data Semantics

| Mode | Behavior | Use Case |
|------|----------|----------|
| **Last-Value** (default) | New value overwrites previous | Config, slow data |
| **Bounded Queue** (opt-in) | Ring buffer, drops oldest on overflow | Video, real-time streams |
| **Queue** (opt-in) | Unbounded, backpressure if consumer is slow | Batch processing, events |

Start with last-value everywhere. Queue semantics opt-in per edge when needed.

## Event Metadata

Minimal metadata per event -- no event sourcing:

```rust
struct EventMeta {
    event_id: u64,
    trace_id: u64,       // correlates related events through the graph
    timestamp: Instant,
    source_node: NodeId,
}

struct Event<T> {
    meta: EventMeta,
    payload: T,
}
```

`trace_id` propagates through the graph: DLL Inject produces frame 4217 with `trace_id: 4217`,
all downstream nodes processing that frame inherit the same trace_id.

### Capture (Opt-In Persistence)

No event sourcing. Instead, individual nodes can opt into capture:

```rust
struct NodeConfig {
    capture: bool,  // default: false
}
```

When enabled, the node's full output is persisted to the database after each execution.

- **Capture off** (default): Zero overhead, only last-value cache
- **Capture on**: Full result written to Postgres/SQLite per execution

Use cases:
- Live debugging: browse captured results in the UI
- Archival: persist production-relevant outputs
- Video inspection: scroll through captured frames with preview

### Data Inspection

Every edge's last-value cache is readable by the UI. Click an edge to see a type-specific preview:

| Edge Type | Preview Widget |
|-----------|---------------|
| `Image` | Inline image / video player |
| `Json` | Tree view |
| `Table` | Data grid |
| `f32` / numeric | Sparkline / value display |
| `String` | Text |
| `Bytes` | Hex view |

Implemented via an `Inspectable` trait:

```rust
trait Inspectable {
    fn preview(&self) -> Element;  // iced widget for inline preview
}
```

## Distributed Execution and Placement

### Runner Constraints

Nodes have placement constraints -- either explicit (user-annotated) or implicit (by node type):

```
[DLL Inject]     -> MUST run on Machine A (has the target process)
[GPU Inference]  -> MUST run on Machine B (has the GPU)
[JSON Transform] -> PREFERENCE: co-locate with heaviest neighbor
```

### Placement Optimization

The system minimizes network hops:

1. Fixed constraints are honored (user-defined or implied by node type)
2. Unconstrained nodes are pulled toward their heaviest data dependency
3. Network crossings happen at optimal boundaries

```
[DLL Inject] -> [Memory Read] -> [Transform] -> [GPU Inference] -> [Postgres Write]
  Machine A      Machine A       Machine A        Machine B          Machine B
  (constraint)   (constraint)    (optimized)      (constraint)       (optimized)
```

One network crossing between Transform and GPU Inference -- the minimum possible.

### Collaboration Visibility

| Layer | Shared? | Via |
|-------|---------|-----|
| Graph definition (nodes, edges, config) | Yes, live | SpacetimeDB |
| Execution state (running, results, logs) | Optional, read-only | SpacetimeDB (aggregated) |
| Execution control (start, stop) | Per-user / per-runner | Local |
| Captured data (full results) | On-demand | Direct request to owner |

Aggregated metrics (throughput, latency, node status) sync to SpacetimeDB periodically.
Full captured data stays local; collaborators can request samples.

## Plugin System

Each domain is a plugin that registers:

- **Node catalog**: Available node types with their pin definitions
- **Pin types**: Domain-specific data types with serialization
- **Execution logic**: How nodes compute outputs from inputs
- **Preview widgets**: `Inspectable` implementations for domain types
- **Subgraph template**: Default layout and configuration

```rust
trait DomainPlugin {
    fn name(&self) -> &str;
    fn node_catalog(&self) -> Vec<NodeDefinition>;
    fn create_node(&self, type_id: &str) -> Box<dyn ExecutableNode>;
}

trait ExecutableNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()>;
    fn pin_definitions(&self) -> &[PinDefinition];
}
```

## Technology Stack

| Component | Technology | Rationale |
|-----------|-----------|-----------|
| Editor UI | iced 0.14 + iced_nodegraph | Existing investment, Rust-native, WASM-capable |
| Collaboration | SpacetimeDB | Real-time shared state, multiplayer-grade sync |
| Local Storage | SQLite | Embedded, zero-config, offline |
| Production DB | PostgreSQL | Robust, extensible, familiar |
| Async Runtime | Tokio | Standard Rust async, needed for runners |
| Serialization | serde + BSATN (SpacetimeDB) | Ecosystem standard + SpacetimeDB wire format |
| IPC | gRPC (tonic) | Cross-machine runner communication |

## Example Flow: D2R Bot Pipeline

```
[Processes]                        [Config: Offsets]
  out: pid (Value)                   out: offsets (Value)
       |                                  |
       v                                  v
[DLL Inject]                       +----------------+
  trigger: pid                     | Unit Parser    |
  out: frame_data (Stream) ------> |  trigger: data |
                                   |  sample: offsets|
[Screen Capture]                   +-------+--------+
  trigger: pid                             |
  out: frame (Stream) --+                  v
                        |           [Bot Logic]
                        +---------> |  trigger: units|
                                    |  trigger: frame|
                                    +---+--------+--+
                                        |        |
                                        v        v
                                [Postgres]   [AI Pipeline]
                                 capture:on   capture:on
```

## Open Questions

- [ ] Subgraph pin exposition UX -- how does the user define which pins are exposed?
- [ ] Hot-reload of plugins -- can domains be added/updated without restarting?
- [ ] Graph serialization format -- JSON, RON, or binary?
- [ ] Authentication model for remote runners
- [ ] Error propagation -- how do node failures affect downstream?
- [ ] Undo/redo granularity in collaborative editing
- [ ] Rate limiting / backpressure for high-frequency streams
- [ ] WASM runner sandboxing for user-defined transform nodes
