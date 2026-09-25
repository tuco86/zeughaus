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

**Who executes** is decided by the store, not negotiated: every top-level
graph (`graph.sub` with `parent == 0`) names its runner in the node's
`runner` column (the `sha256:<hex>` fingerprint of that runner's endpoint),
and each runner executes exactly the subtrees of the graphs that name it
(`zeughaus-runner/src/runner.rs`, `in_scope_of`). Every runner calls
`join_runtime` and announces its pinned URL
(`weida://sha256:<fp>@host:port/`) in its `runtime` row; an editor dials
every runner it finds there. Root-level nodes of a session from before
graphs had owners are adopted once into a new graph by the runner with the
lowest `seq` (`adopt_root_nodes`).

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
| `iced_tabs` | the tab tree the workspace shell uses: sections, one level of groups, drop targets and markers | yes |
| `zeughaus-theme` | the editor's theme: an `iced::Theme` paired with a terminal colour scheme, the catalogs of every widget the editor draws, the bundled pack, the WezTerm scheme parser | yes |
| `zeughaus-transform`, `-flow`, `-graph`, `-ml` | pure plugins | yes |
| `zeughaus-job` | the `job.run` node and the `ProcessHost` trait it executes through; the runner implements the host, the editor registers the plugin detached | yes |
| `zeughaus-capture`, `-db`, `-record`, `-llm` | plugins that touch the OS | native |
| `zeughaus-module` | the SpacetimeDB server module; outside the native workspace, built by `spacetime build` | wasm module |

Dependency direction: plugins depend on core only. The runtime depends on
core. The runner depends on runtime, sync, link, mux, terminal and every
plugin. The widget crates know nothing of the editor's theme: each implements
its catalog for `iced::Theme`, and `zeughaus-theme` implements the same
catalogs for its own type. The editor depends on core, sync, link, mux, the
widget crates, the theme and the plugins it can link -- **not on the
runtime**: it holds node instances for what a node knows about itself, and
gets every value from the runner.

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
in the reducer and locally in every editor. A top-level graph is a
container too: it is what a graph tab shows, and closing its last tab with
its close button deletes it. A nested container's `open` button opens its
contents in a tab of its own.

**Layout.** `AutoLayout` ranks nodes by longest path from a source and orders
a rank by the barycentre of its placed predecessors; a cycle admits the lowest
remaining id as if its incoming edges were not there. It is an ordinary move,
shared through the store.

**Workspace.** The window is undecorated and draws its own titlebar: the
button in the corner moves the tab strip between the titlebar and a sidebar
under it, and the window buttons and edge grips do what the system
decorations would have. Native windows have 10-logical-pixel rounded corners
with a transparent exterior and an opaque workspace. Maximizing removes
the rounding and resize grips; restoring the window brings them back.
The browser canvas stays rectangular.
Below it are tabs of split panes (`iced_tabs::tree`, `pane_grid`). The tab
bar has one section per connected runner, labelled with its host and the
start of its fingerprint, plus `Local` (no store: the scratch graphs) or
`Not running` (graphs whose runner is not connected). A runner section is
that runner's shared workspace: loose tabs and one level of coloured,
collapsible groups, each tab a split tree whose leaves are terminals
(section 9) or graphs, and order, groups, splits and ids come from the
runner. Tabs never move between sections. Section controls add a shell, a
graph (created for that runner) or a group. In both bar placements tabs are
dragged into and out of groups (collapsed ones too), onto another tab's
content to split it, and a pane is dragged by its grip back into the bar to
become a tab (`zeughaus/src/workspace.rs`, `drop_allowed`). A tab dropped on
another tab lands before or after it by the half the pointer is over; the
bar's empty rest after the last section is that section's end, and a drop
onto the place an item already has sends nothing (`drop_command`). Each
pane draws its own graph; the camera is kept per graph, the focused pane's
graph is where the palette spawns nodes, and the selection is dropped when
the focus moves to another graph. Active tab, focus, collapsed sections and
groups, scroll, selection, blink and the tab bar's placement are per window.
Top tabs join the content background, with the close control inside each
tab. The sidebar has the titlebar's chrome and its tabs the same look,
rounded on the left instead of on top. Long labels are shortened; overflow
scrolls.

The icon is one mark: a stencilled Z, cut like the marking on a depot crate.
`assets/icon/render.py` is its geometry and writes `zeughaus.svg` and the two
PNGs anything reads: 256 px compiled into the editor and handed to
`window::Settings::icon`, 64 px as the browser tab's favicon
(`<link rel="icon">` in `index.html`). Below 32 px the generator would have to
drop the stencil bridges, which are then thinner than a pixel; no such size is
needed. The window icon reaches X11 and Windows. It does not reach Wayland:
winit 0.30, which iced 0.14 pins, makes `set_window_icon` a no-op there, and
`xdg_toplevel_icon_v1` -- the protocol that carries an icon per toplevel, and
which KWin implements -- is supported from winit 0.31. Until iced moves, a
Wayland window has no icon, and that stays that way: the editor is one binary
and installs no desktop entry to work around it.

**Themes** (`zeughaus-theme`). One theme type drives everything the window
draws. A `Theme` is an `iced::Theme` -- the widgets' palette -- paired with a
terminal colour scheme: sixteen ANSI colours plus foreground, background,
cursor and selection. The scheme is the single source: a theme loaded from a
file derives its iced palette from it (background, text = foreground,
primary = blue, success = green, warning = yellow, danger = red), and the
editor's own semantics are ANSI slots -- a `Float` pin is green, a `Str` pin
yellow, a `Bool` pin blue, a constant's header the green tinted into the
chrome. The chrome (titlebar, status line) uses the extended
palette's roles; the graph goes through `iced_nodegraph`'s catalog, whose
defaults already derive from the extended palette; the terminal renders the
theme's palette with any entry an OSC 4/10/11 changed laid over it (the
runner's default palette is a constant both ends pin, so "unchanged" means
the same thing on both). The widget crates implement their catalogs for
`iced::Theme`; `zeughaus-theme` implements every catalog for its type by
delegating to the inner iced theme, and `iced_palette`, which only knows
`iced::Theme`, is bridged with `themer`. The bundled pack is every iced
built-in paired with the scheme of the same name from iTerm2-Color-Schemes;
`<state-dir>/themes/*.toml` in WezTerm's format add more. The chosen theme
and the tab bar's placement are per window and persist in
`<state-dir>/editor.toml`.

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

**Tab titles.** The terminal engine reports changed OSC titles to the
runner's mux service independently of screen subscribers. If the title
names a tab or a detached terminal, the runner advances the workspace
revision and publishes a snapshot on the control exchange. No terminal
stream is needed to keep an inactive tab's title current. A user-supplied
tab name takes precedence; otherwise the first non-empty surface names the
tab. Repeated identical OSC titles do not publish another revision.

**Ownership.** The runner owns tab order, splits, ratios, pane and terminal
ids; each editor owns its presentation state. Any client may view a terminal;
exactly one holds its **lease** and may type, resize and move the mouse in
it. The first client that types acquires an unowned terminal, another takes
it with `TakeControl` (`Ctrl+Shift+T`), and a lease survives a network blink
for ten seconds so a redial does not turn a shell read-only. Closing a pane
kills its child; closing an editor window does not.

**Shims and restart.** On unix every terminal's PTY and child live in a shim
process, the runner binary started as `zeughaus-runner shim
<state-dir>/terminals/<id>` (`zeughaus-terminal/src/shim`). It detaches into
a session of its own, starts the profile from `spec.json`, tees a job's log,
keeps the last 4 MiB of output, and serves one session at a time on `sock`
with length-prefixed postcard frames (`Hello`/`Welcome`, `Input`, `Resize`,
`Redraw`, `Close`; `Output`, `Exited`). A new session gets the replay first,
parsed with the terminal's answerbacks muted, then live output, with no gap
between them. Dropping a session leaves the shim running; only `Close` (a
closed pane, `CloseTerminal`) ends it. The mux writes
`<state-dir>/workspace.json` (tabs, groups, splits, id counters, per
terminal its label, grid and job run) after every structural change; a
version 1 file is migrated, its graph panes dropped. A starting runner
reattaches every saved terminal, rebuilds the workspace around the ones that
came back (dead panes removed), gives every graph it owns and no pane shows
a tab, closes shims nothing refers to, and continues the id counters.
`SIGUSR1` saves and `exec`s the
runner's binary again with the same arguments; the incarnation is new, the
terminal and tab ids are the old ones, so an editor keeps its active tab and
focus. SIGINT/SIGTERM still drain and exit, leaving shells in their shims
for the next runner. Windows keeps terminals in the runner process.

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
`Ctrl+Shift+Escape` gives the keyboard back to the app. `Ctrl+Space` never
reaches the shell: the command palette acts on the whole window (tabs,
panes, terminals, the graph), is drawn over whichever tab is in front, and
holds the keyboard while it is open.

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
| editor preferences | `<state-dir>/editor.toml` (`theme`, `tabs`), written by the editor when they change; `<state-dir>/themes/*.toml` are WezTerm colour schemes offered as themes by file name |
| capture backend | the portal is used when `WAYLAND_DISPLAY` is set |
| paths inside nodes | `db.database` `path` and `record.writer` `dir` are settings, resolved against the runner's working directory |
| LLM endpoint | the `base_url` setting on each conversation node (default `http://localhost:1234/v1`) |
| runs | `<state-dir>/runs/<run-id>/` holds `log`, `exit`, `code` and `artifacts/` of every job run this runner executed; `--keep-runs <n>` (default 50) is how many successful runs stay, pruned when a run starts; failed runs and runs without an exit record are never pruned |
| terminals | `<state-dir>/terminals/<id>/` (`spec.json`, `sock`, `shim.log`) per live terminal, `<state-dir>/workspace.json` for the tabs a restarted runner restores |
| editor restart | `SIGUSR1` writes `<state-dir>/restore-<pid>.json` (window size and maximized state, active tab, focused pane and collapsed sections and groups by runner, cameras, node sizes, selection, an open palette with its input, a rename in progress, terminal scroll-back, nested views open in a section without a runner, the scratch document without a store) and `exec`s the editor, which reads and deletes it through `ZEUGHAUS_RESTORE`. The window's position is not restored: a Wayland client can neither read nor set it |

## 12. Testing strategy

- Core and runtime: unit tests on invariants (type agreement, coercion,
  occupancy, change tracking, cycle handling, async holdback, pin sync).
  Integration tests in `zeughaus-runtime/tests` build small graphs from the
  transform plugin and run them.
- Wire formats (`zeughaus-link`, `zeughaus-mux`): round trips, refusal of
  malformed and oversized input, golden bytes for the mux header and kinds.
- Editor: pure functions (layout, wire refusal, relation rules, field
  editing, pending edits, first-dial schedule) are unit-tested; the widget
  tree is verified against the running application. With the `remote`
  feature, `zeughaus --headless --control <socket>` runs the real `App`
  without a window on a hardware wgpu renderer, takes mouse and keyboard
  events from `zeughaus ctl <socket> ...` and writes PNG screenshots with
  the cursor's hover state (`zeughaus/src/remote`), so a check never drives
  the user's desktop. Pointer commands run the loop between their events,
  paced over an optional duration, so a one-command `drag` behaves like a
  hand; `record`/`record-stop` pipe frames at 30 fps through `ffmpeg` into
  an MP4 with the pointer painted in, for showing a change in a PR.
- Shims: `zeughaus-runner/tests/shim.rs` starts real shims through the
  runner binary: a shell survives its session and is reattached with its
  screen, and the replay is bounded to the newest 4 MiB.
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
(trigger) and `cwd: Str` (state; wired, it wins over the setting, which is
how a checkout upstream hands its directory on) in; `ok: Bool`,
`failed: Int` (the exit code, `-1` for a signal or a kill) and `dir: Str`
(the run directory) out. `ok` and `failed` are separate pins so each can
drive its own trigger wire. The node acts only on its own trigger or on a
press (`fire`, which is how `/triggers` and the palette reach it), and
refuses while a run is live ("job busy"), while the runner is held, and in
a process that does not execute. The program sees `ZEUGHAUS_RUN_DIR` (its
run directory, where it may write artifacts directly) and, when the trigger
carried text -- a `trigger` payload, or a string on `run` --
`ZEUGHAUS_PAYLOAD`. The mux stays argv-free: no `TopologyCommand` carries
a command, the graph does. The plugin defines `ProcessHost` (`held`,
`new_run_dir`, `spawn`); the runner implements it in
`zeughaus-runner/src/jobs.rs` over its `MuxService`, the editor registers
`JobPlugin::detached()`, which only contributes the catalog entry.

**A run.** `JobHost::spawn` allocates `<state-dir>/runs/<id>/` (ids continue
past whatever is on disk, so a restart never reuses one), creates `log`, and
starts the process through `MuxService::spawn_owned`: a terminal with no
pane whose PTY bytes are appended to the log before they are parsed, by the
shim that holds the PTY. The node defers the wait to the blocking pool; when
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

**Runs across a restart.** A run's terminal carries its run directory,
start time, `cwd` and artifact globs in `workspace.json`. A restarted runner
adopts every reattached run without an `exit` record (`JobHost::adopt_runs`):
it waits for it like the node would have, then writes `exit` and copies the
artifacts (`zeughaus_job::record_run_end`). The node that started it is gone
with the old process, so its `ok`/`failed` pins do not fire for an adopted
run; adopted runs count as live for a drain.

**Triggers** carry an optional payload (`TriggerRequest { node_id, payload }`)
that becomes the node's `fire` parameter; a bare press sends none. They come
through weida only: no HTTP listener and no polling in the runner.

**The CLI** is the runner binary as a client (`zeughaus-runner/src/cli.rs`):
`zeughaus-runner trigger <endpoint> <node-id> [payload]` pushes on
`/triggers` marked `external`: the run it starts gets a tab in the runner's
locked `Triggered` group, which only the runner fills and from which no tab
is dragged; a press from an editor leaves its run detached.
`zeughaus-runner hold <endpoint> on|off` asks `/hold` and
prints the reply. `<endpoint>` is the pinned root URL a runner prints at
start; the client identity comes from the state directory (`--state-dir`,
`ZEUGHAUS_STATE_DIR`), so a script, a cron entry or a webhook relay is the
same principal as an editor on that machine. No store is involved.

## 14. Not built

Kept here so they are not mistaken for descriptions of the code:

- Staged deployment of graph versions (draft / staged / deployed).
- Placement of nodes across several runners within one graph: a graph runs
  on the runner it names, whole. Edges between graphs of different runners
  are not carried; routing between runners would go over the weida broker,
  not the store.
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
