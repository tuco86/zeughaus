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

### Separation of Editor and Executor -- implemented

Two processes, and the split is not optional: `zeughaus` is an editor that never
executes, `zeughaus-runner` is a headless process that does nothing else.

They meet in the SpacetimeDB store: the editor writes the graph, the runner
reads it, executes it, and publishes results back. Every editor -- the one on
the same machine and one on another continent -- is therefore the same thing, a
remote view. That is what makes a browser editor a plain consequence of the
architecture rather than a special case, and it is what stops a side effect from
happening twice: two editor windows used to mean two runtimes, so a screen
capture node fired once per window, each seeing its own screen.

Which runner executes is decided by the store, not negotiated: every runner
registers in `runtime` and the lowest `seq` owns it, so a second runner is a hot
standby that takes over when the first disconnects.

Results travel over weida: scalars and node errors on the `/events` topic, a
snapshot for late joiners on `/snapshot`, trigger presses on `/triggers`, and
frames on `/samples`. A state store is the wrong pipe for 33 MB per frame, so
nothing a pass produces goes through SpacetimeDB.

### Terminals: a runner-owned multiplexer -- implemented

The runner is also a terminal multiplexer, in the shape of WezTerm's mux and
without its code: `portable-pty` spawns the child, a pinned `wezterm-term`
parses its output into a canonical screen with stable row indices and change
sequence numbers, and `zeughaus-runner/src/mux` serves that screen to every
editor over one `/mux` path. PTY bytes never leave the runner; what travels
is `zeughaus-mux`'s wire model -- rows as spans with wire-stable styles,
deltas of the rows that changed since the sequence number the client holds,
and whole workspace snapshots for the tab and split topology.

Three exchange kinds ride one pooled QUIC connection. A **control** exchange
per client carries the attach (hello, topology, one head per terminal, so the
first paint is one round trip), the structural commands and their replies,
and every later snapshot. A **terminal** exchange per attached terminal is
full duplex: input up, deltas down, neither waiting for the other. **Row
fetches** are short exchanges of their own so a scrollback page cannot block
a keystroke. A delta is computed per subscriber from the last sequence number
that subscriber received, so a slow client gets fewer, larger deltas and never
a queue; output is coalesced at a 12 ms cadence.

Ownership is the plan's: the runner owns tab order, splits, ratios, pane and
terminal ids, and exactly one pane shows the graph; each editor owns its
active tab, focused pane, scroll position, selection and blink. Any client
may view a terminal; exactly one holds its lease and may type, resize and move
the mouse in it. The first client that types acquires an unowned terminal,
another takes it with an explicit command, and a lease survives a network
blink for ten seconds so a redial does not turn a shell read-only. Closing a
pane kills its child; closing an editor window does not. A runner restart is
a new incarnation with an empty workspace: terminals do not migrate.

Security: the runner's identity and one client identity are persisted under
the state directory (`~/.local/state/zeughaus`, `ZEUGHAUS_STATE_DIR`), the
listener requires a trusted client certificate, and a bind on anything but
loopback without configured client trust refuses to start. Every `/mux`
exchange is authorized by the peer identity weida proved; a terminal id is a
name, never a credential. Terminals are created only from runner-side
profiles (the runner user's login shell today); no command carries an argv.
OSC 52 clipboard writes and downloads are not wired and cannot reach the
editor; a hyperlink is only reported, never opened, by a click.

Rendering is one custom wgpu primitive per pane (`iced_terminal`): rows are
shaped once per content and instanced once per palette, a changed row replaces
its arena ranges, and a cursor move touches no row. The font is bundled so the
cell grid is the same on every host.
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

### How containers work -- implemented

Every node carries a `parent` node id; `0` is the root graph. A *container*
node (`NodeDefinition::container`, currently `graph.sub` and `db.database`) has
no pins of its own: its pins in the parent view are synthesized from its direct
children of type `graph.input` (one input pin each) and `graph.output` (one
output pin each), named by that child's `name` setting.

Edges in the store always connect real nodes. A wire drawn onto a container's
pin `x` is stored as an edge to the `graph.input` child named `x` (pin `in`),
and a wire from pin `y` comes from the `graph.output` child named `y` (pin
`out`). Both boundary types are ordinary passthrough nodes, so **the executor
stays flat and knows nothing about nesting**: it sees one graph of real nodes
and real edges, which is what keeps dirty propagation, the trigger/sample
distinction and the type system unchanged by this feature.

The editor is the only component that knows about parents. It filters the
canvas to the graph currently being viewed, maps each edge's endpoints onto the
container that holds them for display, resolves them back to the boundary child
on connect, and offers a way in and out (an `open` button on the container plus
a breadcrumb). Deleting a container deletes its contents -- recursively in the
module reducer, and locally in every editor.

### Planned Domains

| Domain | Node Types | Execution Model |
|--------|-----------|-----------------|
| Process / DLL Injection | Processes, Inject, Memory Read/Write, Hook | Imperative, event-driven |
| Database (FileMaker-style) | Database (container), Table, Insert, Query, SQL | Declarative, SQLite implemented (`zeughaus-db`) -- schema designed in the graph, see below |
| AI / GPU | ONNX Inference, Preprocessing, Postprocessing | Pipeline, batch or stream |
| Workflow / Automation | HTTP, Transform, Filter, Schedule | Sequential, event-triggered |
| Screen Capture / Video | Capture, Encode, Stream, Overlay | Real-time stream |
| Recorder / Dataset | Recorder, Player | Frames plus values as files on disk, implemented (`zeughaus-record`) |

### Database schemas are drawn -- implemented

A `db.table` is not a node with a text field holding a column list; it *is*
the table. Its name is an editable title and its fields are a row editor
(`[name] [type] [remove]` plus `add field`), and every field is a
bidirectional pin spanning the node (`PinDirection::Both` in the core,
`PinSide::Row` in the widget). So a **relation** is a wire drawn between two
field pins -- `orders.customer_id` to `customers.id` -- and it becomes a
`FOREIGN KEY` in the emitted DDL.

That wire carries nothing, and that is the point: **an edge with a
bidirectional end is excluded from execution**, in one place
(`Graph::is_dataflow` in `zeughaus-runtime`, read by `incoming_edges` /
`outgoing_edges` and therefore by the topological order, the dirty walk and
every input set). Two tables referencing each other is a legal schema and a
legal cycle; as a dependency it would leave the whole graph unorderable and
nothing at all would run. The rule is stated on pin declarations, never on
node type ids, so the editor and the runner -- which both build their graph
from the same store -- reach the same answer.

The relations themselves reach the runner as a derived hidden parameter
(`relations`, one `field -> table.field` line each), exactly as `db_path` is
derived from the enclosing `db.database`. Which end of a wire is the
referenced one follows from the fields rather than from the drag: the end
whose field is named `id`, and if neither is, the end the wire was dropped
on. A runner therefore never has to know what a wire between two fields
meant.

The field editor is generic, not database-specific: `SettingKind::Fields {
types }` is a setting kind like `Text` and `Multiline`, its value stays the
plain `name:type` text a multiline field would have held, and the type
vocabulary travels in the setting. That is what lets the browser editor render
the rows for a plugin it cannot even link.

### Machine Learning (Keras) -- implemented

The `zeughaus-ml` plugin turns Keras (TensorFlow) layers into nodes so a neural
network can be designed entirely in the graph and exported as a runnable Keras
functional-API program. A `KerasModel` value -- a directed acyclic graph of
layer steps plus an optional compile config -- flows through the chain: each
layer node consumes a model and emits it extended by one step, mirroring the LLM
plugin's Conversation pattern. A plain feed-forward network is just a DAG where
every step has one input; branches and merges add steps with zero or several.

Codegen uses the functional API so the generated code mirrors the graph edges:
every step becomes a variable (`x0`, `x1`, ...), and the call syntax reflects its
inputs -- none for a root (`x0 = layers.Input(...)`), one for a normal layer
(`x1 = layers.Dense(...)(x0)`), and a list for a merge
(`x3 = layers.Concatenate()([x1, x2])`). The program closes with
`keras.Model(inputs, outputs)` (a list of inputs for multi-input models).

Each step carries a stable identity: the editor node that produced it. So when
one layer fans out into two branches that later merge, the shared step is
emitted once -- the merge dedups branch step sets by id. This is exactly why the
identity must come from the producing node (`ctx.source_node`) rather than a
per-value counter, which would collide across branches.

Layer nodes are data-driven: every supported layer is a row in a static `LAYERS`
table (Input, Dense, Conv1D/2D, Max/Average/GlobalAveragePooling, Flatten,
Reshape, Dropout, BatchNormalization, LayerNormalization, Activation, LSTM, GRU,
Embedding). Merge nodes are a parallel `MERGES` table (Concatenate, Add,
Subtract, Multiply, Average, Maximum, Minimum, Dot) and have two model inputs
`a` and `b`; chain them for more. Each parameter is typed -- strings are quoted
in codegen, raw literals (numbers, tuples, bools) are emitted verbatim, and a
blank value omits the kwarg so Keras applies its own default. The Export node
validates at the boundary (non-empty model, every input branch rooted in an
Input layer) and renders the final Python.

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

x0 = layers.Input(shape=(28, 28, 1))
x1 = layers.Conv2D(filters=32, kernel_size=(3, 3), activation='relu')(x0)
x2 = layers.MaxPooling2D(pool_size=(2, 2))(x1)
x3 = layers.Flatten()(x2)
x4 = layers.Dropout(rate=0.5)(x3)
x5 = layers.Dense(units=10, activation='softmax')(x4)

model = keras.Model(inputs=x0, outputs=x5)
model.compile(optimizer='adam', loss='categorical_crossentropy', metrics=['accuracy'])
model.summary()
```

Branches and merges work the same way. One `Input` fanning out to two `Conv2D`
nodes joined by a `Concatenate` merge node renders as:

```python
x0 = layers.Input(shape=(32, 32, 3))
x1 = layers.Conv2D(filters=16, kernel_size=(3, 3), activation='relu')(x0)
x2 = layers.Conv2D(filters=16, kernel_size=(5, 5), activation='relu')(x0)
x3 = layers.Concatenate()([x1, x2])
x4 = layers.Dense(units=10, activation='softmax')(x3)

model = keras.Model(inputs=x0, outputs=x4)
model.summary()
```

The plugin is pure codegen with no platform dependencies, so it is available in
the wasm editor as well; native runners execute the exported code.

## Type System -- implemented

Pin types are runtime values, not compile-time labels. A pin declares a `Ty`:

```rust
enum Ty {
    Any,                       // wildcard: connects to anything, never coerced
    Bool, Int, Float, Str,     // scalars, bijective with bool/i64/f64/String
    List(Arc<Ty>),
    Option(Arc<Ty>),
    Record(Arc<Record>),       // named, ordered fields -- built at runtime
    Opaque(Arc<str>),          // a plugin's own Rust type, matched by name
}
```

This is what lets a domain describe types nobody wrote in Rust: a user-designed
database schema, a subgraph's exposed interface, an inferred tensor shape. A node
derives its pins from what is connected to it via `sync_pins`, so its interface
can follow its data rather than its source code.

### Types and Values Agree by Construction

A Rust type declares its own `Ty` once, and both the pin declaration and the
runtime tag on values come from that single definition:

```rust
trait Typed: Clone + Send + Sync + 'static {
    fn ty() -> Ty;
    fn repr(&self) -> Repr<'_> { Repr::Opaque }   // structural view for display
}
```

`Value::new::<T>` tags the value with `T::ty()`, so a value's declared type and
its actual payload cannot drift apart -- the failure mode of the previous string
labels, where a pin could claim `f64` while carrying something else. The executor
therefore coerces on the value's own type rather than trusting the source pin's
declaration, which is also what makes an `Any` output (e.g. `flow.hold`) land
correctly on a typed input.

The built-in scalars are deliberately a 1:1 mapping onto Rust types. Narrower
numerics (`u8`, `f32`) are not pin types: a node converts at the emit site, so
there is no ladder of widening converters papering over mismatches. The one
remaining built-in coercion is `Int -> Float`.

### Structural Inspection

`Typed::repr` exposes a borrowed structural view (`Repr`) of scalars, lists and
records. Generic consumers walk it instead of downcasting to every concrete type:
today the editor's in-node value display, later the edge preview widgets and
capture. Nominal plugin types (`KerasModel`, `Conversation`) stay opaque -- their
meaning is their Rust implementation, not a field layout.

Deserializing a value back from its `Repr` is not implemented: `Ty` and
`PinDefinition` are serializable (a subgraph's derived pins must survive
save/load), but reconstructing an arbitrary payload needs a per-type decoder,
which waits for the first consumer (capture or a remote runner).

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
    // Derive pins from what is connected: variadic arity, or a shape that
    // follows an incoming type (a schema, a subgraph interface).
    fn sync_pins(&mut self, connected: &[PinBinding<'_>]) -> bool { false }
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
