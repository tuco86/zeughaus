# Zeughaus - Architecture

A visual dataflow workbench built on [iced](https://github.com/iced-rs/iced)
and `iced_nodegraph`. Heterogeneous tools -- math and string transforms,
screen capture, SQLite schemas, LLM conversations, Keras model design,
recordings -- are composed as one node graph, edited collaboratively, and
executed by a headless process that also multiplexes terminals for its
editors.

This document describes the system as it is. Every section names the crate
that implements it. Ideas that are not implemented are collected at the end
under "Not built", so that nothing above that heading is aspirational.

## 1. Processes and where they meet

```
 editor (zeughaus)          editor (zeughaus, another machine / browser)
   edits, draws               edits, draws
        |   ^                        |   ^
  rows  |   | events, frames,        |   |
        v   | terminals              v   |
 +-------------------+     weida (QUIC, mTLS)      +------------------+
 |  SpacetimeDB      |<---------------------------->|  zeughaus-runner |
 |  node, edge,      |  rows in, presence out       |  executes graph, |
 |  runtime          |                              |  owns terminals  |
 +-------------------+                              +------------------+
```

Two processes, and the split is not optional. `zeughaus` is an editor that
never executes a node; `zeughaus-runner` is a headless process that does
nothing else. That is what makes every editor -- the one on the same machine,
one on another continent, one in a browser -- the same thing, a remote view,
and what keeps a side effect from happening twice: a screen capture fires once
per session, not once per open window.

They meet in two places:

- **The store (SpacetimeDB, `zeughaus-module`, client in `zeughaus-sync`)**
  holds the graph document and runtime presence, and nothing a pass produces.
  Tables: `node` (id, type_id, display_name, x, y, params as JSON, parent),
  `edge` (id, from_node, from_pin, to_node, to_pin), `runtime` (connection,
  identity, auto-incremented `seq`, announced address). Reducers are
  fine-grained (`create_node`, `move_node`, `set_node_params`, `delete_node`,
  `connect_edge`, `disconnect_edge`, `join_runtime`, `announce_endpoint`);
  conflicts are last-writer-wins per row. Ids are made process-unique at
  startup (`NodeId::seed_unique`) so two editors never collide.
- **The link (weida, `zeughaus-link`)** carries everything a pass produces,
  straight from the runner to each editor: scalar outputs, node errors and
  edge traffic on `/events` (pub/sub), a `/snapshot` for a late joiner
  (req/rep), trigger presses the other way on `/triggers` (push/pull), frames
  on `/samples` (one standing exchange per node pin), terminals on `/mux`.
  Only `bool`/`int`/`float`/`str` are values on the wire
  (`zeughaus-core/src/wire.rs`); frames have their own path; everything else
  stays in the runner.

**Who executes** is decided by the store, not negotiated: every runner calls
`join_runtime`, the lowest `seq` owns execution, the others are hot standbys
that take over when it disconnects. The owner announces the pinned URL
(`weida://sha256:<fp>@host:port/`) in its `runtime` row; editors dial that.

**Redial is weida's, resync is ours.** An editor dials each runtime address
once (`zeughaus/src/transport.rs`, `first_dial`) under a `ReconnectPolicy`
(250 ms doubling to 4 s, never giving up). What a redial cannot restore, the
editor does on `PeerEvent::Connected`: a fresh `/snapshot`, reopened feed
exchanges, a fresh mux attach. A runner that restarted is a new peer; the new
address in its `runtime` row is what replaces the tasks.

**Without a store**, the runner refuses to start. The editor starts anyway and
edits a local scratch graph: nothing computes it, the status bar says so, and
the graph is gone on close unless saved as a file (`.zgh`, the JSON
`GraphDocument` in `zeughaus-core/src/document.rs`). Save/Load is an explicit
palette command in both modes; the store is never written from a file behind
the user's back.

## 2. Crates

| crate | role | wasm |
|---|---|---|
| `zeughaus-core` | `Ty`/`Typed`/`Value`, pins, `ExecutableNode`/`DomainPlugin`, settings, converters, scalar wire encoding, store row types | yes |
| `zeughaus-runtime` | `GraphExecutor`: topology, node instances, edge cache, dirty set, async work, node errors | yes |
| `zeughaus-sync` | SpacetimeDB client: generated bindings, `Store` (reconnecting connection), `Session` resolution, ownership queries, reducer calls | native |
| `zeughaus-link` | the runner<->editor protocol over weida: paths, feed, events, snapshot, triggers, run files, hold, and the credentials both ends present | native |
| `zeughaus-mux` | terminal mux wire model: stable ids, workspace topology, rows/deltas, bounded codec, client-side `TerminalView` | yes |
| `zeughaus-terminal` | the runner's terminal engine: PTYs (`portable-pty`) and a pinned `wezterm-term` | native |
| `zeughaus-runner` | the executing process: store loop, executor, weida listener, feed server, mux service, job host | native |
| `zeughaus` | the editor | native + wasm32 |
| `iced_terminal` | terminal widget: one wgpu primitive per pane, input to `TerminalCommand`s, bundled font | native |
| `iced_tabs` | the tab bar the workspace shell uses | yes |
| `zeughaus-transform`, `-flow`, `-graph`, `-ml` | pure plugins | yes |
| `zeughaus-job` | the `job.run` node and the `ProcessHost` trait it executes through; the runner implements the host, the editor registers the plugin detached | yes |
| `zeughaus-capture`, `-db`, `-record`, `-llm` | plugins that touch the OS | native |
| `zeughaus-module` | the SpacetimeDB server module; outside the native workspace, built by `spacetime build` | wasm module |

Dependency direction: plugins depend on core only. The runtime depends on
core. The runner depends on runtime, sync, link, mux, terminal and every
plugin. The editor depends on core, sync, link, mux, the widget crates and the
plugins it can link -- **not on the runtime**: it holds node instances for what
a node knows about itself, and gets every value from the runner.

## 3. Type system (`zeughaus-core`)

Pin types are runtime values, not compile-time labels:

```rust
enum Ty { Any, Bool, Int, Float, Str, List(Arc<Ty>), Option(Arc<Ty>),
          Record(Arc<Record>), Opaque(Arc<str>) }
```

`Any` connects to anything and is never coerced. The scalars map 1:1 onto
`bool`/`i64`/`f64`/`String`; narrower numerics are not pin types, a node
converts at the emit site. `Record` is built at runtime (a user-designed
schema); `Opaque` names a plugin's own Rust type (`KerasModel`,
`Conversation`, `db.table`).

A Rust type declares its `Ty` once through `Typed::ty()`, and both the pin
declaration and the tag on a `Value` come from it, so they cannot disagree.
`Typed::repr` gives a borrowed structural view for generic consumers (the
editor's inline value text); nominal plugin types stay opaque.

Coercion across an edge uses the value's own tag, not the source pin's
declaration (an `Any` output lands correctly on a typed input). Converters are
registered per type pair in `TypeConverters`; the one built-in is
`Int -> Float`. Editor and runner build the same registry from the same
plugins, so "what may connect" and "what is coerced" agree.

## 4. Nodes, pins, settings

`ExecutableNode` is what a plugin implements; `DomainPlugin` lists and
instantiates node types. The catalog is data: a `NodeDefinition` carries the
type id, display name, category, pins, settings and whether the type is a
container. Pins and settings are owned strings, because a node type is not
necessarily authored in Rust (a subgraph, a schema).

**Pins.** `PinDefinition::input(name, ty, kind)`, `::output(name, ty)`,
`::field(name, ty)`. `PinKind::Trigger` declares an event pin -- the node acts
when something arrives on it, and asks `InputSet::changed(pin)` to know that
it did -- while `PinKind::Sample` declares a state pin that is read whenever
the node runs. The editor draws them as squares and circles. Propagation
itself is uniform (section 5). A `field` pin has `PinDirection::Both`: an edge
between two field pins is not dataflow but a declared relationship between
two nodes (section 8).

**Settings** are the one way a node is configured from the editor.
`SettingDef` names a key, a default, a placeholder and a `SettingKind` --
`Text`, `Multiline`, `Title` (the node's own name as an editable heading) or
`Fields { types }` (a `name:type` row editor). The value is always text: the
editor stores it in the node's `params`, the runner hands it to
`set_parameter(key, Value::new(text))`, and the node parses. A node that
refuses a value returns `ZeughausError::InvalidParameter(reason)`; the runner
publishes `SettingRejected` and every editor draws the reason under the field.
Const nodes are ordinary nodes with a `value` setting.

Some parameters are **derived by the editor** rather than typed: `db_path`
(from the enclosing `db.database`), `relations` (from wires between field
pins), `renamed_from` (a table's previous name). They are plain `params` keys
the runner applies like any other.

**Derived pins.** `sync_pins(connected)` lets a node reshape its inputs from
what is wired to it (a variadic merge grows a slot; a recorder takes the
type arriving on `values`). Both processes call it after every connection
change. `refresh_pins` re-reads a node's declaration after a setting changed
it (a table's field list).

**A source needs a clock.** `tick_interval()` lets a node ask to be run
periodically; the host schedules it. `flow.timer` is that clock: without one
upstream, a capture node produces one frame and stops.

## 5. Execution (`zeughaus-runtime`, host loop in `zeughaus-runner`)

`GraphExecutor` owns the graph and the node instances; every mutation goes
through it (`add_node`, `add_edge`, `remove_node`, `disconnect_edge`,
`set_parameter`). It keeps, per edge, the last value and a generation counter,
and per node the last output set and the last error.

A pass (`execute_dirty`) runs the dirty nodes in topological order, computed
once per graph revision. Semantics:

- **Dirty propagation is uniform.** A changed node marks everything
  downstream dirty; every downstream node reruns. A node that must act only on
  its own trigger asks `InputSet::changed(pin)`, which compares the edge's
  cache generation with the one the node saw last.
- **Atomic flush.** A node buffers with `NodeContext::emit` and releases with
  `flush`; multi-output nodes never deliver half a result.
- **Async work.** A node that would block (an LLM request, a portal capture)
  hands `AsyncWork` to the context; the executor marks it pending, holds its
  downstream back, and the host runs it on a blocking pool and delivers the
  result with `deliver_async_result`. A pending node is never dispatched twice.
- **A failing node does not fail the pass.** Its message is recorded, its
  downstream is held back, everything unrelated runs. Errors reach editors as
  `NodeError`/`NodeErrorCleared` events.
- **A cycle does not fail the pass either.** The nodes in it, and everything
  downstream of it, get one node error and are dropped from the dirty set; the
  rest runs. Removing a wire or a node that breaks the cycle wakes them.
- **Relations are not edges to the executor.** `Graph::is_dataflow` excludes
  edges with a `Both` endpoint from ordering and input sets.
- **A new wire is seeded**, not recomputed: `add_edge` copies the source's
  last output into the edge and dirties only the target's subtree, so a node
  with side effects does not re-fire because someone drew a wire.

The runner's loop (`zeughaus-runner/src/runner.rs`) applies store rows to the
executor, diffing parameters against what it last applied so a drag (which
rewrites the row) does not rerun a node; serves clocked nodes; runs a pass;
publishes what changed since the last publish (outputs, cleared pins, errors,
rejections, delivered edges); and answers `/snapshot` from the same state.
Two runners racing for one input pin resolve it identically from the data
(`occupancy_winner`: the larger edge id wins), because arrival order differs
per process.

## 6. The editor (`zeughaus`)

The editor's model is `EditorNode`/`EditorEdge` plus one `ExecutableNode`
instance per node, used only for what a node knows about itself: its pins,
its settings, whether it accepts a value, how it reshapes under connections.
Nothing is executed. Values, errors and setting refusals on screen are what
the runner reported (`remote_outputs`, `remote_errors`, `setting_errors`),
sequence-guarded per pin so a late event never overwrites a fresh one, and
cleared wholesale when the runtime goes away. A pin whose last run produced
no value is drawn dim.

Local edits go to the store through an outbox that replays after a reconnect;
settings edits are held back 400 ms after the last keystroke
(`pending.rs`) and flushed on close. A remote row is applied without echoing
back as a reducer call. Edits made while a window still owes the store a key
are not overwritten by an older shared value for that key.

Connection rules run in one place (`wire_refusal`): while a cable is dragged,
to decide whether the pin under the cursor is a target, and again when a drop
is refused, to say why. A wire that would close a dataflow cycle is refused
at the drop.

**Containers and subgraphs** (`zeughaus-graph`). Every node carries a
`parent`; `0` is the root graph. A container type (`graph.sub`,
`db.database`) has no pins of its own: the editor synthesizes them from its
direct `graph.input`/`graph.output` children, named by each child's title. An
edge drawn onto a container's pin is stored against the boundary child, so
the store and the executor see one flat graph of real nodes; only the editor
knows about nesting. Deleting a container deletes its contents, recursively
in the reducer and locally in every editor.

**Layout.** `AutoLayout` ranks nodes by longest path from a source and orders
a rank by the barycentre of its placed predecessors; a cycle admits the lowest
remaining id as if its incoming edges were not there. It is an ordinary move,
shared through the store.

**Workspace.** The window is tabs of split panes (`iced_tabs`,
`pane_grid`). Exactly one pane shows the graph; every other pane is a
terminal (section 9). Tab order, splits and pane ids come from the runner;
active tab, focus, scroll, selection and blink are per window.

## 7. Plugins

| plugin | nodes | notes |
|---|---|---|
| `transform` | 35: constants, math, trig, logic, compare, string, `display` | `transform.display` is the one node whose body shows data and asks for a frame feed |
| `flow` | `hold` (event -> state), `button` (a manual event, pressed from any editor via `/triggers`), `timer` (the clock), `all` (fan-in: fires once every wired input has fired) | |
| `graph` | `sub` (container), `input`, `output` (boundary passthroughs) | |
| `ml` | Keras layers and merges from static `LAYERS`/`MERGES` tables, `compile`, `export` | a `KerasModel` value is a DAG of steps keyed by the producing node id, so a fan-out that merges emits each step once; `export` renders functional-API Python |
| `llm` | `system`, `user`, `chat`, `last_reply`, `merge` over a `Conversation` value | LM Studio's OpenAI-style endpoint; `chat` is async work |
| `capture` | `screen` | xdg-desktop-portal ScreenCast/Screenshot on Wayland (async, retried while the portal warms up), `scrap` otherwise |
| `db` | `database` (container), `table`, `insert`, `query`, `sql` | SQLite; section 8 |
| `record` | `writer`, `player` | a recording is a directory of `<seq>.png` plus `index.jsonl`; the player is a clocked source |
| `job` | `run` | a process in a terminal the runner owns, log per run on disk; section 13. Registered detached in the editor: same catalog, never executes |

Registration is one list in each process (`Runner::new`, `App::new`), the
same plugins in the same order, native-only ones gated in the editor. A node
type the runner cannot instantiate is a node that never runs; a type the
editor cannot instantiate cannot be placed. `catalog_entry(type_id, name,
category, &instance)` builds a definition from an instance so the two cannot
drift.

## 8. Database schemas are drawn (`zeughaus-db`)

A `db.table` is its schema: an editable title and a `Fields` setting whose
rows are field pins spanning the node. A wire between two field pins is a
relation and becomes a `FOREIGN KEY` in the emitted DDL. The referenced end is
the one whose field is named `id`; if neither is, the end the wire was dropped
on. Field pins carry the column type, and the editor only lets equal types
connect.

The wire carries nothing, and the runtime keeps it out of execution
(`Graph::is_dataflow`), which is what makes two tables referencing each other
a legal schema and not a cycle. The relations reach the runner as the derived
`relations` parameter, exactly as `db_path` is derived from the enclosing
`db.database`. Renaming a table renames it in the file once (`renamed_from`);
removing a field drops the column.

## 9. Terminals: a runner-owned multiplexer

The runner is also a terminal multiplexer, in the shape of WezTerm's mux and
without its code: `portable-pty` spawns the child, a pinned `wezterm-term`
parses its output into a canonical screen with stable row indices and change
sequence numbers (`zeughaus-terminal`), and `zeughaus-runner/src/mux` serves
that screen to every editor over one `/mux` path. PTY bytes never leave the
runner; what travels is `zeughaus-mux`'s wire model: rows as spans with
wire-stable styles, deltas of the rows that changed since the sequence number
the client holds, and whole workspace snapshots for the tab and split
topology. The codec is a fixed frame header (length, protocol version, stable
numeric kind, flags, request id) over `postcard` bodies, every length bounded
before allocation; golden tests pin the header and the kind numbers.

**Three exchange kinds** ride one pooled QUIC connection, told apart by their
first frame. A *control* exchange per client carries the attach (hello,
topology, one head per terminal, so the first paint is one round trip), the
structural commands with correlated, deduplicated replies, and every later
snapshot. A *terminal* exchange per attached terminal is full duplex: input
up, deltas down. *Row fetches* are short exchanges of their own so a
scrollback page cannot block a keystroke. A delta is computed per subscriber
from the last sequence number that subscriber received and carries every
retained row written since, so a slow client gets fewer, larger deltas and
never a queue; output is coalesced at a 12 ms cadence. The client
(`zeughaus-mux/src/view.rs`) applies heads by replacement, deltas only at
their exact base, pages into holes, and keeps its row store bounded around
the viewport.

**Ownership.** The runner owns tab order, splits, ratios, pane and terminal
ids; each editor owns its presentation state. Any client may view a terminal;
exactly one holds its **lease** and may type, resize and move the mouse in
it. The first client that types acquires an unowned terminal, another takes
it with `TakeControl` (`Ctrl+Shift+T`), and a lease survives a network blink
for ten seconds so a redial does not turn a shell read-only. Closing a pane
kills its child; closing an editor window does not. A runner restart is a new
incarnation with an empty workspace: terminals do not migrate.

**Security.** The runner's identity (`runner.pem`) and one client identity
(`client.pem`) live under the state directory (`ZEUGHAUS_STATE_DIR`, else
`$XDG_STATE_HOME/zeughaus`, else `~/.local/state/zeughaus`), owner-only,
written atomically; a PEM that exists but does not parse is an error, never
overwritten. The listener requires a trusted client certificate on every path
(`client.pem` plus any `clients/*.pem`), and a bind on anything but loopback
without client trust refuses to start. Every `/mux` exchange is authorized by
the peer identity weida proved; a terminal id is a name, never a credential.
A client creates terminals only from runner-side profiles (the login shell)
or attaches ones the runner created for a job; no mux command carries an
argv. OSC 52 and downloads are not wired; a hyperlink is reported, never
opened, by a click.

**Rendering** (`iced_terminal`) is one custom wgpu primitive per pane: rows
are shaped once per content and instanced once per palette, a changed row
replaces only its arena ranges, a cursor move touches no row, an idle
terminal schedules no redraw. The font is bundled so the cell grid is the
same on every host. Keys: `Ctrl+Shift+C`/`V` copy and paste,
`Ctrl+Shift+Escape` gives the keyboard back to the app.

## 10. The sample feed

Frames never touch the store. A viewer holds one standing exchange per
(node, pin) on `/samples`: it sends a `FeedRequest` once, naming the size it
draws, and the runner scales the newest frame to a ladder tier (240, 360,
480, 720, 1080 lines; box-averaged, at most 4x4 samples per output pixel) and
writes `FrameHeader`-prefixed frames until the viewer stops reading. Two
viewers of similar size share one scaled result. Backpressure is QUIC's: a
slow viewer gets fewer frames -- always the current one, never a backlog.
The runner keeps an authoritative frame registry per pin and an LRU of scaled
results keyed by (pin, tier, sequence).

## 11. Configuration

| what | where |
|---|---|
| store to join | `zeughaus join <host[:port]/database>`, `zeughaus-runner join <...>`; without it the default session on `127.0.0.1:3000/zeughaus`, and the LAN token is printed for others to join (`Session::resolve` in `zeughaus-sync`) |
| feed/listener bind | `zeughaus-runner --feed-addr <host:port>`; default loopback with an OS-chosen port, so two runners on one host do not collide |
| credentials | `--state-dir <path>` on the runner, else `ZEUGHAUS_STATE_DIR`, else XDG state; a remote editor needs `client.pem` copied into its own state directory |
| capture backend | the portal is used when `WAYLAND_DISPLAY` is set |
| paths inside nodes | `db.database` `path` and `record.writer` `dir` are settings, resolved against the runner's working directory |
| LLM endpoint | the `base_url` setting on each conversation node (default `http://localhost:1234/v1`) |
| runs | `<state-dir>/runs/<run-id>/` holds `log`, `exit`, `code` and `artifacts/` of every job run this runner executed; `--keep-runs <n>` (default 50) is how many successful runs stay, pruned when a run starts; failed runs and runs without an exit record are never pruned |

## 12. Testing strategy

- Core and runtime: unit tests on invariants (type agreement, coercion,
  occupancy, change tracking, cycle handling, async holdback, pin sync).
  Integration tests in `zeughaus-runtime/tests` build small graphs from the
  transform plugin and run them.
- Wire formats (`zeughaus-link`, `zeughaus-mux`): round trips, refusal of
  malformed and oversized input, golden bytes for the mux header and kinds.
- Editor: pure functions (layout, wire refusal, relation rules, field
  editing, pending edits, first-dial schedule) are unit-tested; the widget
  tree is verified against the running application.
- Terminal: engine tests (`zeughaus-terminal`) for styles, Unicode, alternate
  screen, resize, deltas, eviction, exit; the pipeline's CPU side
  (`iced_terminal`) for shaping and instance caches.
- The gate before a push: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`,
  `cargo check --target wasm32-unknown-unknown -p zeughaus`.

## 13. Jobs: processes the runner owns

A `job.run` node (`zeughaus-job`) executes a process with a beginning and an
end -- a CI step, an all-night agent session -- as a terminal the runner
owns. A failed job is therefore a terminal to attach to, not a log to read.

**The node.** Settings `command` (program and arguments on one line, split
like a shell splits words -- quotes group, nothing expands, no shell runs),
`env` (`KEY=VALUE` words, added to the runner's environment), `cwd` (empty:
the runner's), `artifacts` (globs relative to `cwd`), `keep_on_failure`
(default `true`); a malformed setting is `InvalidParameter`. Pins: `run`
(trigger) in; `ok: Bool`, `failed: Int` (the
exit code, `-1` for a signal or a kill) and `run: Str` (the run directory)
out. `ok` and `failed` are separate pins so each can drive its own trigger
wire. The node acts only on its own trigger or on a press (`fire`, which is
how `/triggers` and the palette reach it), and refuses while a run is live
("job busy"), while the runner is held, and in a process that does not
execute. The mux stays argv-free: no `TopologyCommand` carries a command,
the graph does. The plugin defines `ProcessHost` (`held`, `new_run_dir`,
`spawn`); the runner implements it in `zeughaus-runner/src/jobs.rs` over its
`MuxService`, the editor registers `JobPlugin::detached()`, which only
contributes the catalog entry.

**A run.** `JobHost::spawn` allocates `<state-dir>/runs/<id>/` (ids continue
past whatever is on disk, so a restart never reuses one), opens `log`, and
starts the process through `MuxService::spawn_owned`: a terminal with no
pane whose PTY bytes are teed to the log before they are parsed
(`Session::spawn_teed`). The node defers the wait to the blocking pool; when
the child exits, `exit` is written (`code`, `killed`, `started`, `finished`),
declared artifacts are copied under `artifacts/`, and the outputs are
delivered like any async result. A run that exited 0 closes its terminal;
every other outcome keeps it until someone closes it. With
`keep_on_failure` (Unix) the program runs under a five-line `/bin/sh`
wrapper that, on a non-zero exit, writes the code to `<run_dir>/code` and
`exec`s `$SHELL` in the same directory and environment: the run is reported
from the code file while the shell lives on in the terminal, which is what
makes a failed job a place to look rather than a screen to read. The
`flow.all` node is the fan-in: it fires once every wired input has fired
since it last fired, so a job starts when both of its predecessors' `ok`
pins have. The store holds nothing about runs; `/runs` on the runner serves
any run file by range (`RunFileRequest` -> `RunFileReply`,
`zeughaus-link/src/runs.rs`), so logs and artifacts stay on the machine that
produced them.

**Owned terminals in the mux.** Terminal lifetime is separate from pane
lifetime for terminals the runner owns: the workspace snapshot lists them in
`detached` while no pane shows them, `AttachTerminal { terminal, target }`
gives one a pane (a new tab or a split), `ClosePane`/`CloseTab` on such a
pane detaches instead of killing, and `CloseTerminal` kills and forgets.
Profile-spawned terminals keep dying with their pane. The editor's palette
offers "Terminal / Attach" and "Terminal / Close" per detached terminal.

**Hold and drain.** `/hold` (`HoldRequest { held }` -> `HoldReply { held,
live_runs }`) flips the host's flag; the palette offers "Runner / Hold" and
"Runner / Release". SIGINT/SIGTERM hold the runner and let it exit once no
run is live, logging what it is draining; a runner with no live runs exits
at once.

**Triggers** carry an optional payload (`TriggerRequest { node_id, payload }`)
that becomes the node's `fire` parameter; a bare press sends none. They come
through weida only: no HTTP listener and no polling in the runner.

**The CLI** is the runner binary as a client (`zeughaus-runner/src/cli.rs`):
`zeughaus-runner trigger <endpoint> <node-id> [payload]` pushes on
`/triggers`, `zeughaus-runner hold <endpoint> on|off` asks `/hold` and
prints the reply. `<endpoint>` is the pinned root URL a runner prints at
start; the client identity comes from the state directory (`--state-dir`,
`ZEUGHAUS_STATE_DIR`), so a script, a cron entry or a webhook relay is the
same principal as an editor on that machine. No store is involved.

## 14. Not built

Kept here so they are not mistaken for descriptions of the code:

- Staged deployment of graph versions (draft / staged / deployed).
- Placement of nodes across several runners; today the owner runs everything.
  The shape it will take: one runner per machine, each keeping the logs and
  artifacts of the runs it executed and serving them itself; routing between
  runners and editors goes over the weida broker, not the store.
- Opt-in per-node capture of results into a database; the recorder plugin
  writes datasets to disk instead.
- Queue semantics on edges; every edge is last-value. For jobs that means
  one run at a time per node; a queue depth per node, and per-trigger
  instantiation of a pipeline subgraph for parallel branch builds, come when
  needed.
- Rich per-type inspection widgets on edges; nodes show text or a frame.
- A browser editor that syncs with the store; the wasm build edits locally.
- Terminal image protocols; the wire model reserves kinds for them.
- Jobs, decided but not built:
  - A shim process per run that owns the PTY and the log, survives a runner
    restart and is reattached by replaying the log into `wezterm-term`;
    today runs die with the runner, which is why it drains.
  - Reading run files in the editor; `/runs` is served, nothing calls it.
  - Webhooks (Gitea) land on the weida broker once it speaks HTTP and are
    relayed to `/triggers`.
  - `freeze` (SIGSTOP / cgroup freezer) as a per-node hold policy and
    automatic host-state detection (game running, user idle, GPU busy).
  - Secrets through weida's wrapped-secret flow: one refreshable token per
    run, child tokens per service ordered through it, everything invalidated
    when the run ends. Nothing secret-shaped in the store or in settings.
  - Workspace nodes (checkout, btrfs snapshot) producing the `cwd` a job
    runs in; caches per runner under its state directory.
  - VM guests as machines with their own runner. `vm/win11/` is the reference
    QEMU lifecycle for a headless Windows 11 guest; booting it from a job is
    not wired.
