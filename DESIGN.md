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
  edits |   | graph changes, events, |   |
        v   | frames, terminals      v   |
          weida (QUIC, mTLS), one link per runner
        |                              |
        v                              v
 +------------------+          +------------------+
 |  zeughaus-runner |          |  zeughaus-runner |
 |  holds its graph |   ...    |  (another host)  |
 |  executes it,    |          |                  |
 |  owns terminals  |          |                  |
 +------------------+          +------------------+
```

Two processes do the work, and the split is not optional. `zeughaus` is an editor that
never executes a node; `zeughaus-runner` is a headless process that does
nothing else. That is what makes every editor -- the one on the same machine,
one on another continent, one in a browser -- the same thing, a remote view,
and what keeps a side effect from happening twice: a screen capture fires once
per runner, not once per open window. A third, `zeughaus-ci` (section 14),
schedules CI pipelines onto the runners that enlist into it and is dialled by
editors for what it shows.

There is no central store. Everything travels over one weida link per runner
(`zeughaus-link`), on separate paths:

- **The graph (`/graph`, `zeughaus-link/src/graph.rs`)** is the document: nodes
  (id, type_id, display_name, x, y, params as JSON, parent) and edges (id,
  from_node, from_pin, to_node, to_pin), and nothing a pass produces. The
  runner holds it (`zeughaus-runner/src/graphs.rs`). An editor sends
  `Attach { major }` (protocol major `GRAPH_MAJOR`, currently 1) as the first
  frame and gets `Attached { revision, document }`, then a stream of
  `Changed { revision, change }` (`NodeUpsert`, `NodeRemove`, `EdgeInsert`,
  `EdgeRemove`); a revision that skips one ends the exchange and the next
  attach resyncs. Edits go the other way as `Edit { request, edit }`
  (`CreateNode`, `MoveNode`, `SetParams`, `RenameNode`, `DeleteNode`,
  `ConnectEdge`, `DisconnectEdge`); the runner applies them in arrival order
  (last writer wins per node) and answers a refusal with `Refused { request,
  message }` to the sender only. An editor applies the change that comes back,
  not its own edit, so the runner's document is the only truth. Frames are
  length-prefixed JSON, bounded to `MAX_GRAPH_FRAME_BYTES`. Ids are made
  process-unique at startup (`NodeId::seed_unique`) so two editors never
  collide.
- **Runtime traffic** carries everything a pass produces, straight from the
  runner to each editor: scalar outputs, node errors and edge traffic on
  `/events` (pub/sub), a `/snapshot` for a late joiner (req/rep), trigger
  presses the other way on `/triggers` (push/pull), frames on `/samples` (one
  standing exchange per node pin), terminals on `/mux`. Only
  `bool`/`int`/`float`/`str` are values on the wire
  (`zeughaus-core/src/wire.rs`); frames have their own path; everything else
  stays in the runner.

**Who executes** is the runner that holds the graph: it executes every
top-level graph (`graph.sub` with `parent == 0`) of its document and no
other. A graph belongs to the runner it was created on; an editor files each
graph under the runner it received it from and routes every edit of that
graph's nodes to that runner's link.

**Persistence.** A runner writes each top-level graph, with its descendants
and their edges, to `<state-dir>/graphs/<id>.zgh` (the JSON `GraphDocument`
of `zeughaus-core/src/document.rs`), at most once per second and on restart
and stop; the file of a deleted graph is removed. On start it loads every
`graphs/*.zgh`; an unreadable file is logged and left alone, a node or edge
that does not fit (duplicate id, missing parent or endpoint, a top-level node
that is not a `graph.sub`) is skipped with a warning.

**Finding runners.** A runner writes its pinned URL
(`weida://sha256:<fp>@host:port/`) to `<state-dir>/endpoint` after it
started listening. The editor dials the runner in its own state directory's
`endpoint` file and every URL in `remotes = [..]` of
`<state-dir>/zeughaus.toml`, and re-reads both when they change; a URL that
does not parse or pins no peer is logged and skipped. A runner that
disappears from the list loses its graphs in the editor. The CIs are the
URLs in `ci = [..]` of the same file, read the same way; a fingerprint
listed under both is kept as a runner.

**Redial is weida's, resync is ours.** An editor dials each runner once
(`zeughaus/src/transport.rs`, `first_dial`) under a `ReconnectPolicy`
(250 ms doubling to 4 s, never giving up). What a redial cannot restore, the
editor does on `PeerEvent::Connected`: a fresh `/graph` attach and
`/snapshot`, reopened feed exchanges, a fresh mux attach. A runner that
restarted is a new peer with a new address in its `endpoint` file; that file
is what replaces the tasks. Edits made while a link is down wait in the
editor and are sent after the next attach.

**Without a runner**, the editor starts anyway and edits a local scratch
graph: nothing computes it, the status bar says so, and the graph is gone on
close unless saved as a file (`.zgh`). Save exports the focused graph, Load
imports a file as a new graph (fresh ids) into the focused section's runner,
or into the scratch graph when that section is `Local`; both are explicit
palette commands, and no file is read or written behind the user's back.
A runner whose `--feed-addr` is on the WireGuard address fails to start
until `wg0` is up at boot, and the service manager retries.

## 2. Crates

| crate | role | wasm |
|---|---|---|
| `zeughaus-core` | `Ty`/`Typed`/`Value`, pins, `ExecutableNode`/`DomainPlugin`, settings, converters, scalar wire encoding, graph document types | yes |
| `zeughaus-runtime` | `GraphExecutor`: topology, node instances, edge cache, dirty set, async work, node errors | yes |
| `zeughaus-link` | the runner<->editor protocol over weida: paths, graph, feed, events, snapshot, triggers, run files, hold, the CI view, the runner<->CI enlist protocol, and the credentials both ends present | native |
| `zeughaus-mux` | terminal mux wire model: stable ids, workspace topology, rows/deltas, bounded codec, client-side `TerminalView` | yes |
| `zeughaus-terminal` | the terminal engine: PTYs (`portable-pty`) and a pinned `wezterm-term` in the runner; byte-fed sessions and the read-only stream in both runner and CI | native |
| `zeughaus-runner` | the executing process: graph document, executor, weida listener, feed server, mux service, job host, and the executor of the CI jobs it is assigned |
| `zeughaus-ci` | the CI service: pipelines, scheduler, webhook intake, runner registry, stored transcripts; executes nothing |
| `zeughaus` | the editor | native + wasm32 |
| `iced_terminal` | terminal widget: one wgpu primitive per pane, input to `TerminalCommand`s, bundled font | native |
| `iced_tabs` | the tab tree the workspace shell uses: sections, one level of groups, drop targets and markers | yes |
| `zeughaus-theme` | the editor's theme: an `iced::Theme` paired with a terminal colour scheme, the catalogs of every widget the editor draws, the bundled pack, the WezTerm scheme parser | yes |
| `zeughaus-transform`, `-flow`, `-graph`, `-ml` | pure plugins | yes |
| `zeughaus-job` | the `job.run` node and the `ProcessHost` trait it executes through; the runner implements the host, the editor registers the plugin detached | yes |
| `zeughaus-capture`, `-db`, `-record`, `-llm` | plugins that touch the OS | native |

Dependency direction: plugins depend on core only. The runtime depends on
core. The runner depends on runtime, link, mux, terminal and every
plugin. The widget crates know nothing of the editor's theme: each implements
its catalog for `iced::Theme`, and `zeughaus-theme` implements the same
catalogs for its own type. The editor depends on core, link, mux, the
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

The runner's loop (`zeughaus-runner/src/runner.rs`) applies document changes to the
executor, diffing parameters against what it last applied so a drag (which
changes the node) does not rerun a node; serves clocked nodes; runs a pass;
publishes what changed since the last publish (outputs, cleared pins, errors,
rejections, delivered edges); and answers `/snapshot` from the same state.
Input occupancy is resolved identically by every process from the data
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

Local edits go to the runner of their graph as `Edit` frames; the editor
applies the `Changed` frame that comes back, without echoing it again.
Settings edits are held back 400 ms after the last keystroke
(`pending.rs`) and flushed on close. Edits made while a window still owes
the runner a key are not overwritten by an older shared value for that key.

Connection rules run in one place (`wire_refusal`): while a cable is dragged,
to decide whether the pin under the cursor is a target, and again when a drop
is refused, to say why. A wire that would close a dataflow cycle is refused
at the drop.

**Containers and subgraphs** (`zeughaus-graph`). Every node carries a
`parent`; `0` is the root graph. A container type (`graph.sub`,
`db.database`) has no pins of its own: the editor synthesizes them from its
direct `graph.input`/`graph.output` children, named by each child's title. An
edge drawn onto a container's pin is stored against the boundary child, so
the runner and the executor see one flat graph of real nodes; only the editor
knows about nesting. Deleting a container deletes its contents, recursively
in the runner and locally in every editor. A top-level graph is a
container too: it is what a graph tab shows, and closing its last tab with
its close button deletes it. A nested container's `open` button opens its
contents in a tab of its own.

**Layout.** `AutoLayout` ranks nodes by longest path from a source and orders
a rank by the barycentre of its placed predecessors; a cycle admits the lowest
remaining id as if its incoming edges were not there. It is an ordinary move,
shared through the runner.

**Workspace.** The window is undecorated and draws its own titlebar: the
button in the corner moves the tab strip between the titlebar and a sidebar
under it, and the window buttons and edge grips do what the system
decorations would have. Native windows have 10-logical-pixel rounded corners
with a transparent exterior and an opaque workspace. Maximizing removes
the rounding and resize grips; restoring the window brings them back.
The browser canvas stays rectangular.
Below it are tabs of split panes (`iced_tabs::tree`, `pane_grid`). The tab
bar has one section per connected runner, labelled with its host (and its
port, or the start of its fingerprint, where two runners share one host), plus `Local` (no runner: the scratch graphs) or
`Not running` (graphs whose runner is not connected), and after the runners
a `CI <label>` section for each CI the editor dials (section 14). A runner section is
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
`zeughaus/assets/icon/render.py` is its geometry and writes `zeughaus.svg`
and the two PNGs anything reads: 256 px compiled into the editor and handed to
`window::Settings::icon`, 64 px as the browser tab's favicon
(`<link rel="icon">` in `index.html`). Below 32 px the generator would have to
drop the stencil bridges, which are then thinner than a pixel; no such size is
needed. The window icon reaches X11 and Windows. It does not reach Wayland:
winit 0.30, which iced 0.14 pins, makes `set_window_icon` a no-op there, and
`xdg_toplevel_icon_v1` -- the protocol that carries an icon per toplevel, and
which KWin implements -- is supported from winit 0.31. Until iced moves, a
Wayland compositor finds the icon through the desktop entry
`deploy/install.sh` installs, named after the window's `app_id`. macOS has no
window icon at all: the icon of an app is the Dock's, and macOS reads it from
the `Info.plist` of the bundle the app runs in. `deploy/macos-app.sh` is that
bundle -- `deploy/macos/Info.plist` and an icns `render.py` draws at the ten
sizes `iconutil` wants, around a release build of the editor -- and it is the
way the editor has an icon on macOS, as it is for every other terminal there.
A binary run outside a bundle (`cargo run`, `~/.cargo/bin/zeughaus`) has
nowhere to read one from, so it hands AppKit the 256 px PNG itself through
`NSApplication::setApplicationIconImage:`, once the window is open: the Dock
makes the tile for a process while AppKit launches, and an image handed over
earlier is gone by the time the tile appears. Inside a bundle that call is
skipped, since the icns is the better icon.

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
delegating to the inner iced theme. The command palette is the editor's
own view over `iced_palette`'s filtering, drawn with the editor's theme.
Secondary text (`Theme::muted`) and status words (`Theme::readable`) are
mixed only as far as they keep 4.5:1 against the canvas. The bundled pack is every iced
built-in paired with the scheme of the same name from iTerm2-Color-Schemes;
`<state-dir>/themes/*.toml` in WezTerm's format add more. The chosen theme
and the tab bar's placement are per window and persist in
`<state-dir>/editor.toml`.

**The face** of the chrome is bundled (`zeughaus/src/font.rs`):
ComicShannsMono Nerd Font Propo, Nerd Fonts v3.5.1, the default font of the
application and of the headless host, so a capture shows the letters the
user sees. It is also the icon set: codicons and the Material circles are
glyphs of that face at their own width. Terminals keep the Mono variant of
`iced_terminal`, whose cell grid needs one advance for every glyph.

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
| `job` | `run` | a process in a terminal the runner owns, a timestamped WAL per run on disk; section 13. Registered detached in the editor: same catalog, never executes |

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

**History is the runner's.** A shell keeps 20 000 rows of scrollback in the
runner's `wezterm-term` model; an editor holds at most 1 000 rows per
terminal around its viewport (`ROW_CAPACITY`), gets 128 rows above the
screen with a head, and fetches the rest when it scrolls there. Stable row
numbers belong to one screen: the primary and the alternate screen count
theirs independently, so every switch between them starts a new epoch, and
the runner answers it with a fresh head instead of a delta. Fetches and
pages name their epoch; a stale one is refused.

**Keys and alerts.** A key travels as semantics (key, modifiers, press or
release) and is encoded in the runner, where the modes are known:
`wezterm-term` encodes the legacy xterm forms itself (Shift+Enter becomes a
line feed, which xterm cannot tell from Enter otherwise), and kitty's
keyboard protocol, which a child negotiates in-band, is encoded with
wezterm's own encoder in `zeughaus-terminal/src/convert.rs`. On the
alternate screen the wheel goes to the runner too and becomes cursor keys
for a child that does not read the mouse. OSC 9 and OSC 777 notifications
are ordered events like the bell; the editor shows them on the desktop
unless the window has the focus and its focused pane is that terminal. An
OSC 8 link is opened by the platform's handler on an explicit Ctrl+click,
for `http`, `https` and `file` only.

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
a session of its own, starts the profile from `spec.json`, writes the
terminal's WAL, keeps the last 4 MiB of output, and serves one session at a time on `sock`
with length-prefixed postcard frames (`Hello`/`Welcome`, `Input`, `Resize`,
`Redraw`, `Close`; `Output`, `Exited`). A new session gets the replay first,
parsed with the terminal's answerbacks muted, then live output, with no gap
between them. Dropping a session leaves the shim running; only `Close` (a
closed pane, `CloseTerminal`) ends it.

**The WAL.** Every terminal's byte stream is written to a WAL with a time
per chunk (`zeughaus-terminal/src/wal.rs`): the magic `ZGHWAL1\n`, then
records of `u64 LE` microseconds since the epoch, `u32 LE` length and the
bytes one PTY read returned, stamped by the shim before anything else
touches them. A reader stops at the last complete record, so a torn tail is
not an error. The times sit on the byte stream, not on lines; a line's time
is derived from the chunk it ends in. A shell's WAL is `wal` in its shim
directory, rotates at 64 MiB into `wal.1` and goes with the terminal; a
job run's is `<run_dir>/wal` and stays with the run. The mux writes
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

**Locale.** Shells and jobs inherit the runner's environment, and the runner
fixes its locale before any thread exists
(`zeughaus-terminal/src/locale.rs`): the locale variables of its own
environment when it has any, else the first readable `locale.conf`
(`$XDG_CONFIG_HOME`, `~/.config`, `/etc/locale.conf`, `/etc/default/locale`)
or on macOS the `AppleLocale` default mapped onto `locale -a` (language to
`LANG`, region to `LC_NUMERIC`/`LC_TIME`/`LC_MONETARY`). A value `locale -a`
does not list is dropped, a non-UTF-8 `LC_ALL` becomes `LANG`, and a
non-UTF-8 character type gets a UTF-8 `LC_CTYPE`. The editor's locale plays
no part; shims already running keep theirs.

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
same on every host at one scale. A cell is drawn in whole device pixels, so
the editor hands each pane its window's scale factor (asked for when the
window opens, updated on `Rescaled`) and the pane counts its columns with
the cell rounded at that scale; a pane keeps its grid a logical pixel off
its side edges. Keys: `Ctrl+Shift+C`/`V` copy and paste,
`Ctrl+Shift+Escape` gives the keyboard back to the app; on macOS `Cmd+C`/
`V`/`T`/`Escape` do the same, Command never reaches the child, the left
Option is Meta and the right Option composes like AltGr. `Ctrl+Space`
(and `Cmd+Shift+P` on macOS) never reaches the shell: the command palette
acts on the whole window (tabs, panes, terminals, the graph), is drawn over
whichever tab is in front, and holds the keyboard while it is open.

## 10. The sample feed

Frames never travel on `/graph`. A viewer holds one standing exchange per
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
| runners to dial | the local runner's `<state-dir>/endpoint` (written by the runner at start) plus `remotes = ["weida://..."]` in `<state-dir>/zeughaus.toml`; both are re-read when they change. A graph lives on the runner it was created on and in its `<state-dir>/graphs/<id>.zgh` |
| feed/listener bind | `zeughaus-runner --feed-addr <host:port>`; default loopback with an OS-chosen port, so two runners on one host do not collide |
| credentials | `--state-dir <path>` on the runner, else `ZEUGHAUS_STATE_DIR`, else XDG state; a remote editor needs `client.pem` copied into its own state directory |
| editor preferences | `<state-dir>/editor.toml` (`theme`, `tabs`), written by the editor when they change; `<state-dir>/themes/*.toml` are WezTerm colour schemes offered as themes by file name |
| capture backend | the portal is used when `WAYLAND_DISPLAY` is set |
| paths inside nodes | `db.database` `path` and `record.writer` `dir` are settings, resolved against the runner's working directory |
| LLM endpoint | the `base_url` setting on each conversation node (default `http://localhost:1234/v1`) |
| runs | `<state-dir>/runs/<run-id>/` holds `wal` (a run from before the WAL: `log`), `exit` and `artifacts/` of every job run this runner executed; `--keep-runs <n>` (default 50) is how many successful runs stay, pruned when a run starts; failed runs and runs without an exit record are never pruned |
| terminals | `<state-dir>/terminals/<id>/` (`spec.json`, `sock`, `shim.log`, a shell's `wal` and `wal.1`) per live terminal, `<state-dir>/workspace.json` for the tabs a restarted runner restores |
| editor restart | `SIGUSR1` writes `<state-dir>/restore-<pid>.json` (window size and maximized state, active tab, focused pane and collapsed sections and groups by runner, cameras, node sizes, selection, an open palette with its input, a rename in progress, terminal scroll-back, nested views open in a section without a runner, the scratch document without a runner) and `exec`s the editor, which reads and deletes it through `ZEUGHAUS_RESTORE`. The window's position is not restored: a Wayland client can neither read nor set it |
| deployment | `deploy/install.sh`: binaries in `~/.cargo/bin`, the systemd user unit `zeughaus-runner` (`KillMode=process`: the shims outlive the runner process; `reload` is `SIGUSR1`), and `~/.local/share/applications/net.doodleshnookie.Zeughaus.desktop` with its icon, which is how a Wayland compositor shows the editor's icon. Where `systemd-run --user --scope true` succeeds at start, the runner starts every shim in a transient scope of its own, so systemd-oomd, which kills whole cgroups, takes one terminal instead of the unit with every shell |
| self-update | `<state-dir>/update.toml` (`channel`, optional `url`) turns the runner's updater on (section 14, **Releases and the updater**); it works in `<state-dir>/updates/<build>/`. Without the file nothing is checked or fetched |
| maintenance | `<state-dir>/maintain.toml` (`[[task]]`: `name`, `command` argv, `every_minutes`, `timeout_minutes`, default 30) lists tasks the runner runs itself (section 13, **Maintenance**); `<state-dir>/maintain/<name>.json` records each one's last start. Without the file nothing runs |

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
owns, which an editor can attach while the process runs. The terminal ends
with the process, and what it said is in the run's WAL.

**The node.** Settings `command` (program and arguments on one line, split
like a shell splits words -- quotes group, nothing expands, no shell runs),
`env` (`KEY=VALUE` words, added to the runner's environment), `cwd` (empty:
the runner's), `artifacts` (globs relative to `cwd`); a malformed setting is
`InvalidParameter`, and a setting the node no longer has (`keep_on_failure`
in an old graph) is ignored. Pins: `run`
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
past whatever is on disk, so a restart never reuses one), creates an empty
WAL `wal`, and
starts the process through `MuxService::spawn_owned`: a terminal with no
pane whose PTY bytes are appended to the WAL before they are parsed, by the
shim that holds the PTY. The node defers the wait to the blocking pool; when
the child exits, `exit` is written (`code`, `killed`, `started`, `finished`),
declared artifacts are copied under `artifacts/`, and the outputs are
delivered like any async result. A run is over when its program is, with the
program's exit code, and its terminal closes whatever the outcome; the WAL
holds what it showed. A restarted runner closes the terminals of recorded
runs the same way.
Readers go through `jobs::run_output`, which also reads the raw `log` of a
run from before the WAL.
The `flow.all` node is the fan-in: it fires once every wired input has
fired since it last fired, so a job starts when both of its predecessors' `ok`
pins have. The graph holds nothing about runs; `/runs` on the runner serves
any run file (`wal`, `exit`, `artifacts/<rel>`) by range (`RunFileRequest` -> `RunFileReply`,
`zeughaus-link/src/runs.rs`), so output and artifacts stay on the machine that
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

**One runner per state directory.** A serving runner holds an exclusive
`flock` on `<state-dir>/runner.lock` (with its pid in it) and refuses to
start when another holds it: both would reattach the same shims, a shim
serves only the newest connection, and the second runner would take the
first one's terminals and leave them unreachable when it exits. The file
is close-on-exec, so a restart's `exec` releases it to the new image.
Arguments the serving mode does not know, `--help` included, never reach
it: the binary prints its usage instead.

**Triggers** carry an optional payload (`TriggerRequest { node_id, payload }`)
that becomes the node's `fire` parameter; a bare press sends none. They come
through weida only: no HTTP listener and no polling in the runner.

**The CLI** is the runner binary as a client (`zeughaus-runner/src/cli.rs`):
`zeughaus-runner trigger <endpoint> <node-id> [payload]` pushes on
`/triggers` marked `external`: the run it starts gets a tab in the runner's
locked `Triggered` group, which only the runner fills and from which no tab
is dragged and which exists only while it holds a tab (the next run creates
it again); a press from an editor leaves its run detached. The group shows
runs that are still going: a run's terminal closes when its program ends.
`zeughaus-runner hold <endpoint> on|off` asks `/hold` and
prints the reply. `<endpoint>` is the pinned root URL a runner prints at
start; the client identity comes from the state directory (`--state-dir`,
`ZEUGHAUS_STATE_DIR`), so a script, a cron entry or a webhook relay is the
same principal as an editor on that machine. No graph is involved.
`zeughaus-runner mcp` is the subcommand that serves instead of asking once:
see "The CI over MCP" in section 14.

**Maintenance** (`zeughaus-runner/src/maintain.rs`). One thread reads
`<state-dir>/maintain.toml` every 60 s, after 30 s at start; a file that
does not parse or hold (bad or duplicate name, empty `command`, a period
outside 1 to 10080 minutes, a deadline outside 1 to 1440) is logged once as
`[maintain] <path>: <error>` and no task runs. A task is due when
`maintain/<name>.json` is missing or `every_minutes` have passed since its
`started`. It runs only when the runner is not held and has no live run: as
a run of the job host (`<exe> timeout <secs> -- <command..>`, label
`maintain <name>`, working directory the home directory, `external`, so a
terminal in the `Triggered` group while it runs), one task at a time.
While it runs the host counts as held: a CI `prepare` waits and a graph job
is refused, so a toolchain update never swaps files under a build. A task
that does not end with code 0 is logged as `[maintain] <name>: exited N |
timed out | killed; log <run>/wal` and sent as `RuntimeEvent::Alert` on the
`machine` topic, which every editor shows as a desktop notification.
`zeughaus-runner maintain` prints one line per task from the files alone,
so it runs beside the serving runner: `<name>  every <m> min  <never | ok |
exit N | timed out | killed | running>  <age> ago  <run dir>`.

## 14. CI: pipelines from a repository's `.zeughaus-ci/` folder

`zeughaus-ci` (`zeughaus-ci/`) is a third process: a service that schedules
pipelines onto the runners that enlist into it and keeps their records and
transcripts. It executes nothing. A runner executes: with an
`<state-dir>/enlist.toml` it dials the CI and runs what it is assigned as
jobs of its own `JobHost` (`zeughaus-runner/src/enlist/`). An editor lists CIs
as `ci = ["<endpoint>"]` next to `remotes` in `zeughaus.toml`. In production:

| host | process | Unix user | address | enlisted as |
|---|---|---|---|---|
| sadala (aarch64) | `zeughaus-ci serve` | `zeughaus-ci` | weida `10.8.0.1:7444`, HTTP `127.0.0.1:8686` | - |
| sadala | runner | `zeughaus` | `10.8.0.1:7443` | `sadala` |
| tuco-pc | runner | `hannes` | `10.8.0.10:7443` | `tuco-pc-hannes` |
| tuco-pc | runner: containers, Windows VM, GPU busy | `zeughaus` | `10.8.0.10:7444` | `tuco-pc` (default runner) |
| atik (macOS) | runner, launchd agent | `uebelacker` | `10.8.0.7:7443` | `atik` |
| wohnzimmer (Windows 10) | runner, boot task | `zeughaus` | `10.8.0.12:7443` | `wohnzimmer` (`os = "windows"`) |

**Jobs are scripts.** Every `.sh`/`.ps1` file in `.zeughaus-ci/` with a TOML header
between `# /// ci` and `# ///` is a job named after its file. The header
holds these keys:

- `on`: `push <glob>`, `tag <glob>`, or `cron <5 fields>`;
- `needs`: exactly one of `on` and `needs` is set;
- `image` (a `.zeughaus-ci/<image>.Containerfile`), `machine` (a runner or
  a Windows VM of one, by name) or `platform` (a key of `ci.toml`
  `[platforms]`, exclusive with `machine`); none means the host of the
  default runner;
- `context`: jobs of a pipeline that name the same one share a workspace and
  run one after another (**Contexts**);
- `cache`, `secrets`, `when_busy` (`wait`/`freeze`/`run`), `env`,
  `timeout_minutes`.

Files without a header are helpers. `zeughaus-ci check` validates a folder and
`zeughaus-ci plan` shows what an event selects, both without a service
(`header.rs`, `pipeline.rs`). The selection is the jobs whose `on` matches the
event, plus every job whose `needs` are all selected.

**Where a job runs** is decided once, when its pipeline is set up
(`scheduler.rs`, `place_job`), and kept in the job's record as `runner` and
`place`. `ci.toml` names the runners: `default_runner` and one
`[runners.<name>]` each with the `fingerprint` of the runner's own pinned
URL, its `os` (`unix`, or `windows`), the `machines` it boots and the
`wait_minutes` (15) a job waits for it.
`[platforms]` maps a platform name (`[a-z0-9][a-z0-9-]*`, e.g.
`linux-amd64`) to a runner or a machine of one; `ci.toml` is refused when a
value is neither. A job's target is the `[platforms]` entry of its
`platform` (a platform missing there fails the setup), else its `machine`,
else none:

- `image = I`: a container on the target runner, on the default runner when
  there is no target. A target that is a Windows VM is an error.
- the target is a runner: that runner's host. The job is `.sh`, or `.ps1`
  on a Windows runner, which also takes no image.
- the target is a machine in some runner's `machines`: that machine on that
  runner. The job is `.ps1`.
- no target: the host of the default runner.

`when_busy = "freeze"` must resolve to a container or a machine; a host
placement is an error, since nothing can pause it. The job's environment has
`CI_PLATFORM` when it has a `platform`.

A name that is neither a runner nor a machine of one, or the wrong script
kind, fails the pipeline's setup like an invalid header.

**Events are files.** A webhook (`serve --hook <addr>`, HMAC-checked, Caddy on
sadala forwards `https://ci.doodleshnookie.net/hook/<repo>` to it),
`zeughaus-ci run` and the cron schedules all write `ci/inbox/*.json`. The
scheduler moves each inbox file to
`ci/queue/<repo>.<kind>.<hash(ref, cron)>.json`, overwriting what waits
there. A newer push to a ref therefore replaces the one queued behind a
running pipeline and never cancels the running one. Without `--hook` the
service has no HTTP listener and pipelines come from `run`, cron and try
uploads (**Try runs**, whose events are keyed by upload: none replaces
another).

**Pipelines** (`scheduler.rs`, one thread):

1. Fetch into a bare mirror `ci/mirrors/<repo>.git`, with the token in the
   environment and never in argv.
2. Pin the commit as `refs/ci/<n>`.
3. Read `.zeughaus-ci/` at that commit.
4. Record every decision in `ci/pipelines/<repo>/<n>.json` before acting
   on it.

**Channels** group a repository's pipelines (`channels.rs`).
`.zeughaus-ci/channels.toml` lists `[[channel]]` tables of `name` and `on`
patterns; the first channel whose `on` matches the event gets the pipeline
(`release` on `tag v*`, `nightly` on `cron *`, `dev` on `push main` in
zeughaus's own file). Without
a match, or without the file, a push lands in a channel named after its
branch, a tag in `tags`, a cron in `cron`, and records from before channels
are grouped the same way. The channel is decided in setup and kept in the
record; an invalid file fails the setup like an invalid job header. `check`
lists the channels, `plan` starts with the one an event selects, and
`status` shows it per pipeline. Retention counts per channel (**Disk**).
`clean = true` on a channel marks the pipelines it gets `clean` (kept in the
record): their jobs start every workspace from nothing (**Contexts**).

**The enlist protocol** (`zeughaus-link/src/enlist.rs`, `ENLIST_MAJOR` 3) is
one long exchange on `/enlist` (req/rep) per runner. Frames are a 4-byte
little-endian length and a postcard body, at most `MAX_ENLIST_FRAME_BYTES`
(1 MiB), refused before the body is allocated.

- The runner opens with `Hello { major, name, offer, runs }`: its `Offer`
  (`os`, `arch`, `cpus`, whether it has `containers`, its `machines`) and a
  `RunReport` for each run in its `assigned/` directory. The CI checks the
  TLS principal against the `fingerprint` of the `[runners.*]` entry that
  `name` names, the major, and that the runner's `os` is the host
  `ci.toml` gives it (`windows` or not), and answers `Refused { message }` or
  `Welcome { major, offsets }`, where each offset is how many bytes of that
  run's output the CI already holds.
- The runner then reports `Busy`, `Machine` (a machine's status), `Started`,
  `StartFailed`, `Output { run, offset, bytes }` (at most
  `MAX_OUTPUT_CHUNK`), `Frozen` and `Finished { run, end }`; the CI sends
  `Assign`, `Kill`, `Upload` and `Forget`. A run is named
  `<repo>.<number>.<job>` (`run_id`).
- `Assign` carries everything the run needs and the runner has not: the
  repository URL and the fetch token, the commit, the script, the place, the
  environment (`CI`, `CI_REPO`, `CI_REF`, `CI_SHA`, `CI_JOB`, `CI_PLATFORM`, ...), the
  granted secrets, the timeout, `when_busy`, the inputs, the name of the
  workspace and how to check it out (`Checkout`). The runner adds
  the paths and the CPU count (`CI_WORKSPACE`, `CI_OUTPUT`, `CI_INPUTS`,
  `CI_CPUS`, `CARGO_BUILD_JOBS`).
- Output is the run's `wal` byte for byte. The CI appends a chunk when its
  offset is the stored length, appends only the part beyond the length when
  the offset is below it (a resend after a reconnect), and ends the exchange
  when it is above: the runner reconnects and resumes from the new
  `Welcome`. Output of a run the CI did not assign to that runner is
  refused.

The runner keeps the connection (`link.rs`): it redials after 1 s, doubling
to 30 s, starting over after a connection that lasted 60 s. It presents its
own `runner.pem` as the client identity, and the CI's URL in `enlist.toml`
pins the CI. Both sides log it: `[enlist] connected to <ci> as <name>`,
`refused: <message>`, `lost: <cause>`.

**A run on the runner** (`run.rs`). `Assign` is prepared on a thread of its
own, after the runner is no longer held (`zeughaus-runner hold`):

1. Fetch the mirror `ci/mirrors/<repo>.git` (the CI's own copy is a separate
   one), check the commit is there, pin it as `refs/ci/<n>`. A try run has
   no commit on the forge: it downloads its bundles into `<run>/bundles/`
   instead (**Try runs**).
2. Stage the inputs into `<run>/inputs/<need>/`: hard-linked from this
   runner's `ci/artifacts/<run>/` when the need ran here, else downloaded
   through the CI's relay into a temporary tar of its own and unpacked.
3. Write the secrets as files under `$XDG_RUNTIME_DIR/zeughaus-ci/<run>/`
   (`ci/tmp/<run>` where there is no `XDG_RUNTIME_DIR`, as on macOS), which
   the launcher removes.
4. Render `launch.sh` and start it as a `JobHost` run marked `external`, so it
   shows up in that runner's `Triggered` group. Persist
   `ci/assigned/<run>.json` (the assignment without secrets and fetch
   token) and send `Started`.
5. Deliver the log every 200 ms from the offset of the last `Welcome`. When
   the run ends and its log has stopped growing, move a non-empty
   `<run>/artifacts` to `ci/artifacts/<run>/` (on success), remove
   `<run>/inputs` and send `Finished { code, killed, timed_out, artifacts }`.

A run is over when its program is. `inner.sh` replaces itself with the job
and `launch.sh` ends with its exit code (70 for a checkout that failed, 71
for an image build, 75 for the copy into the guest), so a failed run's
terminal closes like a successful one's and nothing waits behind it: the log
is the record, `Finished` follows once it has settled, and `<run>/inputs`
goes with the run. For a machine job the guest's run directory goes when the
launcher ends (on success, once the output is copied back).

The launcher puts a persistent workspace `ci/work/<repo>/<workspace>`, named
after the job or after its `context`, into the state the assignment's
`checkout` asks for and starts the job in it. `Clean` (the default) checks
the commit out and cleans the tree except the `cache` paths, `Wipe` does
the same keeping no path, and both then write the pipeline's number to
`.git/zci-context`. `Continue` neither fetches, checks out nor cleans: an
earlier job of the pipeline's context left the workspace, and the launcher
ends with exit 70 (`context workspace <ws> no longer holds pipeline #<n>`)
when `.git/zci-context` does not name this pipeline, as it does not after
the runner's budget removed the workspace or another pipeline's job took it
in between. Then it starts the job:

- **Host:** directly, under `zeughaus-runner timeout <seconds> -- <program>`
  (a process group that gets `SIGTERM`, `SIGKILL` 60 s later, exit 124;
  macOS has no `timeout`). On a Windows runner `launch.ps1` replaces
  `launch.sh` and `inner.sh`: the same checkout, the secrets dot-sourced from
  `env.ps1` and deleted, the job under `timeout`, whose job object ends the
  whole process tree at the deadline with 124 and when the launcher ends (a
  closed terminal signals nothing on Windows), and the script's exit code is
  the job's. The Windows runner has no shims:
  it holds every process it starts in a job object of its own, so its
  terminals and runs end with it, and a run a restarted runner finds no
  terminal for is recorded as killed.
- **Container:** `podman run --rm -it` with the workspace at `/work` and the
  inputs read-only. On macOS the engine is Apple's `container` instead
  (`stage::Engine`, chosen by the host OS, no setting): the launcher runs
  `container system start --enable-kernel-install` when `container system
  status` fails, builds the image if `container image inspect` does not know
  its tag, and `container run --rm -it --init` with the same mounts. The
  system is never stopped by the runner; every container is a VM of its own
  that `--rm` removes. `container` has no read-only mount, so the inputs are
  writable there.
- **Machine:** over ssh, from a git bundle, in `W:\work\<repo>\<workspace>`.

Host and container jobs time out inside `inner.sh`; a machine job on the
runner's own clock (`control.rs`), which counts only unfrozen seconds, writes
`<run>/timeout` and kills the local ssh. `Kill` closes the run's terminal.

**Artifacts** go through `CI_OUTPUT`. An artifact set exists once, on the
runner that made it, and only as long as a job needs it; there is no
retention count on either side. A runner keeps a successful run's set and
hard-links it as `CI_INPUTS/<need>/` for a job of its own that needs it. For a
job on another runner, the CI is a relay: the consumer's runner opens
`Download { run }` on `/artifacts` (req/rep, `ArtifactRequest`/`ArtifactReply`,
at most `MAX_ARTIFACT_BYTES`), the CI checks that it is assigned a job that
needs the run and sends the producing runner `Upload { run, transfer }`;
that runner packs the set into a temporary tar and answers on an exchange of
its own with `Upload { run, transfer, size }` and the bytes, which the CI
streams into the waiting download as they arrive and never stores (or
`Unavailable`, which fails the download at once; nothing for ten minutes
fails it too). A consumer is assigned only while the producing runner is
connected. Once every job that needs a set has ended (succeeded, failed or
skipped), and at once for a set nobody needs, the CI sends the producing
runner `DropArtifacts { run }`; a runner's `Hello` lists the sets it holds,
and the CI drops each one no pending job needs, which covers a drop lost
with a connection or a restart of the CI. Nothing else reads a finished set.
Only configured runners may use `/artifacts`. `TryBundle { run, index }` is
the one request the CI answers from its own disk: bundle `index` of a try
run, to the runner that run is assigned to (**Try runs**).
Commit statuses (`zeughaus/<job>`) go from the CI to GitHub or Forgejo
through `curl`, with the token on stdin.

**A failure says why.** When a run ends, the CI reads the end of its stored
WAL, removes the escape sequences (`excerpt.rs`) and keeps the last 40
lines, trailing blank ones dropped: the job's own last words. The excerpt
goes into the job's record. The commit status carries its first line that
looks like a cause
(`panicked at`, then `error[`, `error:`, `FAILED`) instead of the bare exit
code, unless the job holds secrets, whose output stays on the CI. `zeughaus-ci
status` prints the excerpt under every failed job of each repository's
newest pipeline and the job's runner, and `zeughaus-ci log <repo> <n> <job>
[--tail N]` prints a job's whole output as plain text; both read the state
directory, so on sadala they go through the `/usr/local/bin/zeughaus-ci`
wrapper, like `run`, `forge-check` and `secrets-check`.

**Transcripts stay on the CI.** The output a runner sends is stored as
`ci/pipelines/<repo>/<n>.runs/<job>/wal`, and the end of the run as
`.../exit` (`code=<code or none>`). They go with the pipeline record, not
with any runner's `--keep-runs`.

**The default branch has an owner.** `streak.rs` derives from the records,
per job of the default branch (push and cron pipelines), the first
pipeline of its current run of failures, and the last pipeline in which
every deploy job (one without `on`) succeeded. `zeughaus-ci status` leads with
it (`griasdi main: windows red since #138 (79e9035, 17 h, 13 pipelines); last
publish #136`). A failed job in a streak that began in an earlier pipeline
posts `red since #<n>: <cause>`. The pipeline that turns a job red, and
every red pipeline once a job has been red for an hour, raise an alert: a
line in the journal and `RuntimeEvent::Alert` on the `ci` topic, which
every connected editor shows as a desktop notification. A push to the
default branch whose commit has no `.zeughaus-ci/` is recorded as a
`no-jobs` pipeline and posts an `error` on `zeughaus/pipeline`, naming a
leftover `.ci/`; on other refs that stays silent.

**Waiting, skipping, failing.** A job whose needs succeeded is `Waiting`
while its runner is not connected (`waiting for runner <R>`), is `Skipped`
after the runner's `wait_minutes` without it (`runner <R> offline`; the
forge hears `success`, so an offline Mac never turns a branch red), and
waits while the runner is busy and its `when_busy` is not `run` (`runner <R>
busy`), or while its workspace (one job at a time per repository and runner)
or machine is in use, or its context is held (**Contexts**). A container place
on a runner that offers none, or a machine it does not list, fails the job. A
job whose need's artifacts are on another runner waits for that runner like
for its own (`waiting for runner <R> (artifacts of <need>)`, skipped after its
`wait_minutes`).

**Contexts** let jobs of one pipeline share one warm workspace. Jobs with
the same `context` use the workspace named after it and run one after
another in the pipeline's record order (topological, ties by name), so each
finds what the one before built, `cache` paths or not; every other job
has the workspace of its own name. `load_jobs` refuses a context that is
also a job's name, jobs of one context that differ in `image`, `machine`,
`platform` or `cache`, and the name `zeughaus-try`, which no job or
context may take: it is the workspace of **Try runs**. A pipeline keeps its
context from its first job to its last (`workspace_wait`): while another
pipeline's job is active in the workspace the job waits (`workspace <ws> in
use by #<n>`), and so it does while an earlier job of its own pipeline has not
ended (`waiting for <job> in context <ws>`) or another pipeline holds the
context (`context <ws> held by #<n>`). The context goes to the pipeline that
began in it (one of its jobs started), else to the oldest, so a younger
pipeline never holds back an older one that has not begun and no two wait for
each other. A job after one of its context that started gets
`Checkout::Continue`, and in a pipeline whose channel is `clean` the first job
to use a workspace gets `Checkout::Wipe` (`build_assignment`).

**Try runs** run a command on a runner of another host in the working trees of
an editor's agent: uncommitted files and the sibling path dependencies of its
checkout (`../weida`) included. The agent snapshots its checkout and each
dependency as a commit without a parent (the tree `git add -A` sees; the
main one also has the command as `.zeughaus-ci/zeughaus-try.sh`, `.ps1` for a
Windows runner) and bundles each under one ref of `refs/zeughaus-try/`. The
CI's `/try` service (`tryrun.rs`, wire types in `zeughaus_link::tryrun`) takes
one exchange per run from an editor's key (a runner's is refused): a
`TryUpload` frame, then the bundles as raw bytes, answered `Accepted { repo,
number }` or `Refused { message }`.

- The upload names the `origin` of the main checkout, the `target`, an
  optional `image`, `timeout_minutes` (1 to 1440), the uploader (`actor`), the
  command (`summary`) and the checkouts: directory name, commit and bundle
  size, at most `MAX_TRY_DIRS` (8), each bundle at most `MAX_TRY_BUNDLE_BYTES`
  (512 MiB). A name that is no plain name, a commit that is not 40 hex digits
  and a size over the limit are refused before a byte is stored.
- The repository is the `ci.toml` entry whose `url` names the same repository
  as `origin` (compared in lowercase, without scheme, user, trailing `/` and
  `.git`, an scp-style `host:path` as `host/path`), else the last segment of
  the URL when that is a name no entry has: a try needs no entry in `ci.toml`.
- The bundles are stored as `ci/try/<id>/<i>.bundle`, then `upload.json`; an
  event of kind `try` with `upload = <id>` goes to the inbox, and the
  exchange waits up to 30 s for the scheduler to write the pipeline's number
  to `ci/try/<id>.number` (beside the directory, which goes at once with a
  pipeline that does not start). A try posts no commit status, raises no red
  alert and cannot be rerun: its trees were the uploader's. Its upload and
  the image mirror below go when its pipeline ends; directories and number
  files a week old are swept when the CI starts.

`Scheduler::set_up_try` makes the pipeline: channel `try`, `sha` the main
checkout's commit, the command's first line as subject, and one synthesised
job, `try`: `zeughaus-try.sh` (`.ps1` on a Windows runner, where no image
is possible), context `zeughaus-try`, the upload's timeout, `when_busy =
"wait"`, no secrets, cache or needs. The `target` is a `[platforms]` key,
else a runner or machine name, and `place_job` places the job on that
runner's host, or in a container when the upload has an `image`, whose
Containerfile is read from the snapshot's first bundle, fetched into the bare
mirror `ci/mirrors/<repo>.try.git`. A target that is a Windows VM ends the
pipeline as `error` (`try runs on a runner's host or in a container;
<target> is the machine <vm>`): a VM's workspace is one bundle, copied into
the guest. The context makes the tries of a repository take turns on a
runner, in the order they came (**Contexts**).

The assignment has no forge access and no secrets, `workspace = zeughaus-try`
and an `Upload { dirs }`. The runner (`run.rs`) skips the mirror, downloads
bundle `i` of each dir into `<run>/bundles/<i>.bundle` (`TryBundle`) and
renders `launch.sh` (`launch.ps1` on Windows) with one block per dir: enter
`<root>/<dir>` below the workspace, `git init` if new, `git fetch --prune` the
bundle's `refs/zeughaus-try/*`, `git checkout --force --detach <sha>` and `git
clean -ffdq`. There is no `-x`, so ignored files such as `target/` stay warm
from one try to the next, and the main checkout's `../weida` is the sibling
checkout of the same upload. Then it removes the bundles, touches
`<root>/.zci-last-used` (the marker the runner's budget reads when there is
no `.git` in the workspace) and starts the job in the main checkout:
`CI_WORKSPACE` is `<root>/<main>` on the host, and in a container, which
mounts the root at `/try`, `/try/<main>`. The runner refuses a try that has
no checkouts, more than `MAX_TRY_DIRS`, a name or commit that is not plain, a
machine place or a workspace other than `zeughaus-try`.

**Restarts.** The CI persists a record before acting, and a run lives on its
runner, so either side may restart:

- A restarted CI reloads its running pipelines; their active jobs keep status
  and runner until the runner reports.
- A runner's `Hello` settles its jobs: a `Starting` job the runner does not
  know, assigned before this connection, goes back to `Pending` and is
  assigned again; a `Running` job it does not know fails (`runner <R> lost
  the run`); a reported run the CI has no job for is told `Kill` and
  `Forget`; a reported run that already ended is handled as `Finished`;
  every listed artifact set no pending job needs is told `DropArtifacts`.
  `Welcome.offsets` resume the output where the CI's copy ends.
- A runner that is gone (`RunnerGone`) leaves its jobs alone. One that stays
  away past the job's timeout plus 15 minutes fails them (`runner <R> lost`);
  one that comes back before reports the runs and they go on.
- A restarted runner reloads `assigned/*.json`, reattaches each run's terminal
  (`adopt_runs`), waits for the `exit` record of those without an end, and
  delivers log and `Finished` after the next `Welcome`. An `Assign` it had
  not started is lost, and the CI assigns it again. It thaws whatever a
  previous process froze. `Forget` removes a run's file.

**Busy and freeze** belong to the runner that has a `[busy]` table in its
`enlist.toml`. `busy.rs` samples `nvidia-smi` every 10 s. The runner counts
as busy once the mean over `busy_after_seconds` reaches `gpu_percent`, and as
free again once the mean over `free_after_seconds` is below it. While busy,
the CI assigns it only `when_busy = "run"` jobs. Running `freeze` jobs of that
runner are paused, with `podman pause` for a container and HMP `stop` on the
VM's `monitor.sock` for a machine, and resumed when it is free; the runner
reports each as `Frozen`. Apple's `container` cannot pause a container: on a
macOS runner the attempt is logged (`cannot freeze: apple container cannot
pause a container`) and the job runs on.

The measurement can be overridden. `Busy` holds the GPU's verdict and a
mode (`auto`, `busy`, `free`; `<state-dir>/ci/busy-mode` of the runner, so a
restart keeps it); the runner acts on the mode, or under `auto` on the
measurement. `/busy` (`BusyRequest { mode }` -> `MachineState { mode, busy
}`, `zeughaus-link/src/machine.rs`) on the runner sets the mode. The runner
publishes every change as `RuntimeEvent::Machine` on the `machine` topic,
carries the current state in the snapshot, which it answers only once that
state is in, and sends it to the CI as `Busy`. So the editor's section of
that runner shows a control that cycles auto, busy, free: a play circle
while free, a pause circle while busy, filled when the mode is set by hand.
A runner without `[busy]` reports no machine and has no control.

**Machines** (`machine.rs`, a runner's `[machines.<name>]`) are Windows VMs
(`kind = "windows-vm"`) that run `.ps1` jobs. One is booted on the first job
that needs it (`vm.sh run`, then `prepare.ps1` in the guest). It runs one job
at a time, and `vm.sh stop` shuts it down after `idle_minutes` without a job.
The launcher copies the bundle, the inputs and an `env.ps1` into the
guest, checks the commit out in `W:\work\<repo>\<workspace>`, runs the job
under `ssh -tt`, copies `out/` back after a success and removes the run
directory whatever the outcome; its exit code is the job's.
The runner reports each machine's status to the CI as `Machine`.

**The CI view.** `zeughaus-ci serve` is a weida peer for editors too: `/ci`
(`CiRequest` -> `CiReply`, `zeughaus-link/src/ci.rs`, `serve.rs`), `/events`,
`/snapshot` (always `machine: None`, no outputs) and `/mux`. An editor's
key is under `<state-dir>/clients/`; a runner's is pinned by `ci.toml`, and
each service admits only the kind of peer it is for. `/ci` answers an
overview (every channel with its newest pipeline, and the machines: every
configured runner in name order as `kind = "runner"` with `offline`, `online`,
or `busy`/`free` (with ` (set)` when the mode is set by hand), then each
machine the connected runners report as `kind = "windows-vm"`, each with the
jobs active on it as `MachineJob { repo, channel, number, job }`, and the
`[platforms]` of `ci.toml` as (platform, target) pairs), a channel's pipelines
newest first by pages, one pipeline, opens or closes a transcript, answers
`Log { repo, number, job, tail }` with the last lines of a job's stored output
as plain text (at most 5000 lines and 3 MiB, read from the end of its `wal`,
`finished` once its `exit` exists) and takes two actions, both answered
`Accepted`:
`Rerun { repo, number }` writes the pipeline's event to the inbox again,
pinned to the recorded sha, with the actor `rerun of #<n>` (a try run is
refused: its trees were the uploader's); `Cancel { repo,
number }` leaves `ci/cancel/<repo>.<n>` for the scheduler, which saves
`cancelled` in the record, sends `Kill` to every active job and skips the
rest (`cancelled`), records a killed job as skipped rather than failed, and
ends the pipeline as `cancelled`: no red alert, error statuses
`cancelled (#<n>)` on the forge, and the red streak ignores it. A failed
job's `JobView` carries its `cause` line (the one the forge status shows,
but also for jobs that hold secrets: editors are trusted peers and can
open the transcript anyway) and the stored `excerpt`. Every saved record is announced as
`RuntimeEvent::CiPipeline` on the `ci` topic. In the editor
(`zeughaus/src/app/ci.rs`) each URL of `ci` is a link of its own, and its
section `CI <label>` comes after the runners: a `Runners` tab with a table of the
runners and machines (kind glyph, name, state dot and word, running jobs),
whose running jobs open their transcripts, then one locked group per
repository with a tab per channel, titled with the channel (the group names
the repository), whose accent dot is the tone of the channel's newest
pipeline. A channel tab lists its pipelines in a scrollable, newest on top
and separated by rules, one row each: a status glyph (codicons from the
bundled Nerd Font, so status never rests on colour alone), `#<n>` and the
commit's subject (the CI reads it from its mirror when the pipeline starts,
and once at start for records without one), the branch, tag or cron, the
commit and the actor, the cause line of each failed job, and the duration
and age (the local time on hover). A closed row ends in one glyph per job.
The newest pipeline and every running one are open unless the user closed
them, and any other opens on a press of its row: an open pipeline adds
`Rerun` and, while it runs, `Cancel`, its note, its jobs as cards laid out by
their `needs` (status glyph, name, and how long and where, or what it waits
for; outlined by status), and for each failed job its excerpt from the cause
line on and an `Open log` button. What a rerun or cancel came to is said in
the toolbar, and so is a CI that does not answer (an error, or no overview
for 15 s): the views then show its last report. The overview also lists the
queued events (`QueuedView`, each under the channel of the newest pipeline
with its queue key), which a channel shows as `Queued` rows above its
pipelines. Up and Down move a cursor over the list in front, Enter opens or
closes the pipeline under it, `R` reruns it, Shift+`C` cancels it and `L`
opens the log of its first failed job (letters no widget took), and Escape
closes the transcript when no palette or rename takes it first. The
list is at most 1200 px wide. While the channel in front has something running, a
one-second clock moves its elapsed times. The cables between the cards are one
`iced_nodegraph_sdf` primitive under them; nothing pans, zooms, moves or
takes the wheel, so the list scrolls like any other. Pressing a job
opens its transcript in a pane beside the list, and its card is tinted with
the accent while it is open: the CI creates a
`Session::fed` (`zeughaus-terminal`), a screen that a follower task feeds
from the stored `wal` every 200 ms and that ends with the `exit` file, and
serves it on its own `/mux` through `zeughaus_terminal::serve::stream_terminal`,
so the editor shows it through the ordinary terminal stream, live,
read-only. The CI's mux has no workspace, no shells and ignores input except
a resize: the transcript pane sizes the screen without controlling it
(`iced_terminal::Terminal::sizing`), and the CI rewraps it to the pane that
asked last, so it fills the pane and its lines wrap at the pane's edge. It
keeps at most 16 transcript screens across editors and closes the least
recently opened one first; an editor asks again for its open transcripts
after the CI restarts. The section's ids are the editor's own
(`SurfaceRef::Ci`), and it takes no structural command. An editor's link to
a CI takes no workspace, graph or feed of its own: only `/events`,
`/snapshot`, `/ci` and `/mux`.

**The CI over MCP** (`zeughaus-runner/src/mcp/`). `zeughaus-runner
[--state-dir <dir>] mcp [--ci <url>]` is one more client of `/ci`, for an
agent: a Model Context Protocol server on stdin and stdout that the agent
starts and keeps for its session. It executes nothing and holds no graph. It
dials the CI that `--ci` names, else the first entry of `ci = [..]` in
`<state-dir>/zeughaus.toml` (the editor's file, read without insisting on
its other keys), which must pin a fingerprint, with that directory's
`client.pem`: the CI sees the principal an editor of the directory is. With
no CI or no key the server still starts and every tool call is a result
flagged `isError` that names what is missing. `rpc.rs` is the JSON-RPC 2.0
half, one message per line: `initialize` (the client's protocol revision
when it is 2024-11-05, 2025-03-26 or 2025-06-18, else 2025-06-18), `ping`,
`tools/list` and `tools/call`; notifications are ignored, an unknown method
is `-32601` and a batch is refused. Every `tools/call` is a task of its
own and the answers go through one locked writer, because an agent's
subagents share the process and a call may wait for minutes; when stdin
ends, the calls still running finish and answer, then the process exits 0.
Logs go to stderr: stdout carries nothing but protocol.

The tools (`tools.rs`) are `ci_status` (the overview and the `[platforms]`
of `ci.toml`, which are what a `try_run` target resolves through),
`ci_pipelines`, `ci_pipeline` (a failed job adds its `cause` and
`excerpt`), `ci_log` (`CiRequest::Log`: the last lines of a job's stored
output, 5000 at most, and whether the job has finished), `ci_rerun`,
`ci_cancel`, `try_run` and `try_wait`; their answers are one fact per line
in a fixed shape, and a CI that answers with an error, or not at all within
30 s, is the tool's error.

`try_run` runs a command on another runner from
the working tree it is called in. `snapshot.rs` makes, for the checkout of
the call and for each Cargo path dependency beside it (the packages of
`cargo metadata` without a source whose manifest is outside the checkout;
one that is not in a git checkout with the same parent directory is
refused, because the runner lays the checkouts out side by side), one
commit without a parent: in a temporary index, `HEAD`'s tree, then `git add
-A`, so with every change and every untracked file the ignore rules do not
exclude, and in the main checkout the two job files
`.zeughaus-ci/zeughaus-try.sh` and `.ps1` that `cd` to the directory of the
call and run the command. The checkout's own index, branch and refs are not
touched. Each commit travels as a bundle of one ref, at most 512 MiB and at
most 8 checkouts, in one exchange on `/try` (`TryUpload`, then the bundles
as raw bytes, answered with `TryReply`); the CI's refusal is the tool's
error. The CI numbers the pipeline of the channel `try`, and the tool looks
at it every 2 s until it has finished or `wait_seconds` (540; at most 600,
because the client's own limit for a call is 660 s) have passed since the
call began, snapshot and upload included, and answers `<repo> #<n> try on
<target>: <status> (<secs> s)`, what the CI says of the job and the last 200
lines of its output; a run that is still going is answered with `still
running after <s> s; call try_wait with repo=.. number=..`. `try_wait`
waits from its own start. The `origin` remote names the repository (the
CI matches it, so credentials in a remote's URL are cut before it is sent)
and `actor` is `<user>@<host>`.

**Disk.** The CI keeps the 200 newest pipeline records per channel, with
their transcripts (running ones never go), and no artifacts. A runner
deletes workspaces beyond its `budget_gb` (50) least recently used first,
through `podman unshare rm` because podman containers leave subordinate-uid
files (plain removal under Apple's `container`, whose virtiofs writes files
the user owns), removes image tags no pipeline uses (`podman rmi`,
`container image delete` on macOS), deletes an artifact set when the CI
drops it, and a run's `inputs/` when the run ends.

**Secrets** (`secrets.rs`) live in one OpenBao KV v2 document
(`[secrets]` in `ci.toml`, `secret/zeughaus/ci`); `ci.toml` only names
them. The CI, its hook and the CLI log in with the AppRole
`zeughaus-ci`, whose secret_id is sealed with `systemd-creds` to the host
and the CI user (`deploy/ci/openbao.sh`, run on sadala, creates and rotates
it). The token stays in the process; a document is read at use time and kept
for `cache_ttl_seconds`, never served stale, so a value rotated in OpenBao
takes effect without a restart. When OpenBao cannot be reached, fetches,
job starts and statuses fail with its message and the hook answers 503,
which the forge redelivers. `zeughaus-ci secrets-check`, and every start of
`serve`, list each named secret as present or missing. A job gets only
those its repository's `grants` release to the event. They travel in
`Assign` to the runner the job is placed on and reach the job through files
there.

**Releases and the updater.** zeughaus's own `.zeughaus-ci/` produces the
releases its installed runners follow. Per platform one build job, named
after it, builds the editor and the runner in a full build on its own
platform, with no cross compilation of Rust code, and packs
`zeughaus-<version>-<platform>.tar.gz` (`.zip` on
Windows) into `CI_OUTPUT`, with the workspace version and `LICENSE-*` in a
top directory `zeughaus-<version>-<platform>/`. The build number
`CI_PIPELINE` is compiled in as `ZEUGHAUS_BUILD`; a binary made anywhere else
has build 0.

| job | `platform` | where (`[platforms]` of the production `ci.toml`) |
|---|---|---|
| `linux-amd64` | `linux-amd64` | tuco-pc, container `trixie`, `freeze` |
| `linux-arm64` | `linux-arm64` | atik, the same Containerfile in an Apple `container` |
| `windows-amd64` | - | `machine = "wohnzimmer"`, the Windows runner's host (the `windows-amd64` platform stays on the `win11` VM, which griasdi's job needs) |
| `darwin-arm64` | `darwin-arm64` | atik's host; the editor as `Zeughaus.app` (`deploy/macos-app.sh`) and the runner |
| `publish` | - | `machine = "sadala"`, needs the gate and all four |

The Linux archives also hold `zeughaus-ci`; the updater never installs it.
`build.sh` (no header, so a helper) is sourced by the Unix build jobs and by
`publish.sh`. The build jobs and the gate run on a push to `main`, a `v*`
tag and the cron `0 3 * * *`; the gate also runs on every other push.

`publish.sh` needs an aarch64 host: it extracts `zeughaus-runner` from the
`linux-arm64` archive and runs `zeughaus-runner release bundle` over the
four archives, then `release verify`. The directory it makes holds the
archives, `manifest.json` (format 1: channel, build, version, commit, and
per platform file, size and SHA-256), `manifest.sig` (the raw Ed25519
signature of those exact bytes) and `SHA256SUMS`. Its channel follows the
event (push `dev`, cron `nightly`, tag `release`; a tag must equal
`v<version>`). It is copied to `/var/www/zeughaus-updates/<channel>/<build>/`
(through a temporary name and a rename), `latest` is rewritten with the
build number, and all but the two newest builds of the channel go. A rerun of
an older pipeline leaves `latest` where it is. Caddy on sadala serves the
directory at `https://zeughaus.doodleshnookie.net/updates/` (`handle_path
/updates/*` with `file_server`, `latest` sent with `Cache-Control:
no-cache`). For cron and tag events the bundle is also attached to a GitHub
release of `tuco86/zeughaus`: `nightly-<date>` as a prerelease (the seven
newest stay) for the cron, `v<version>` with the version's CHANGELOG section
as its body for a tag, a prerelease when the version has a `-`. The two
secrets this needs are in the OpenBao document next to the others and
granted to `push main`, `cron *` and `tag v*`: `ZEUGHAUS_UPDATE_KEY` (64 hex
characters, the Ed25519 seed from `zeughaus-runner release keygen`) and
`ZEUGHAUS_GITHUB_TOKEN`. `publish.sh` reaches GitHub through a header file,
never through argv.

**What a runner trusts** is its own binary: the public key that signs
releases is compiled in (`TRUSTED_KEYS` in `update/manifest.rs`), so a server
that answers cannot make a runner install anything the CI's key did not
sign. The signature is over the manifest, which binds the build number, the
channel and the size and SHA-256 of every archive; an old release served as
a new one, another channel's and a tampered archive are refused.

**The updater** (`zeughaus-runner/src/update/`, unix only) starts with the
runner when `<state-dir>/update.toml` exists. Its keys are `channel` and
`url` (default `https://zeughaus.doodleshnookie.net/updates`); an unknown key
or a malformed file is logged as `[update] <path>: <error>` and means no
updater. One thread waits 10 s, then looks every 60 s at
`<url>/<channel>/latest` with `curl`. A number above the binary's own build
starts an attempt in `<state-dir>/updates/<build>/`: manifest and signature,
then the archive of the host's platform (`linux-amd64`, `linux-arm64`,
`darwin-arm64`; an Intel Mac has none and logs that it does not update),
unpacked with `tar`. Nothing outside
`updates/` changes before every check has passed. Then the runner file is
replaced by a rename of a copy written next to it, together with the editor
installed beside it: `zeughaus` in the runner's directory on Linux,
`~/Applications/Zeughaus.app` on macOS (the bundle written as `.new`, the
old one moved aside); where there is none, only the runner is replaced, and
if one of the writes fails nothing is replaced. Editors are then sent
`SIGUSR1`, which reopens them as they were (Linux: every process whose
`/proc/<pid>/exe` is that file, or `(deleted)`, and whose `SigCgt` shows the
handler, as `deploy/install.sh` checks; macOS: the pids `pgrep -f
<app>/Contents/MacOS/zeughaus` finds), and the runner restarts itself through
the same path as its own `SIGUSR1` (section 9). Every other directory under
`updates/` is then removed; a failed attempt is logged as `[update] build
<n>: <error>`, removed, and not retried by that process.

The updater is off where `update.toml` is absent, and that is the setting for
the CI's own hosts: sadala and the `tuco-pc` service runner have none, so a
broken `dev` build cannot take down the CI or the runner that would build
and publish its fix. They change by `deploy/install-sadala.sh` and
`deploy/install.sh`. The runners of the user's own machines (`tuco-pc-hannes`,
atik) follow `dev`. Windows has no updater; `release bundle` and `release
verify` exist on every platform.

## 15. Not built

Kept here so they are not mistaken for descriptions of the code:

- Staged deployment of graph versions (draft / staged / deployed).
- Placement of nodes across several runners within one graph: a graph runs
  on the runner that holds it, whole. Edges between graphs of different runners
  are not carried; routing between runners would go over the weida broker.
- Opt-in per-node capture of results into a database; the recorder plugin
  writes datasets to disk instead.
- Queue semantics on edges; every edge is last-value. For jobs that means
  one run at a time per node; a queue depth per node, and per-trigger
  instantiation of a pipeline subgraph for parallel branch builds, come when
  needed.
- Rich per-type inspection widgets on edges; nodes show text or a frame.
- A browser editor that reaches runners; the wasm build edits locally.
- Terminal image protocols; the wire model reserves kinds for them.
- Jobs of the graph (`job.run`), decided but not built; CI (section 14)
  has its own webhook intake, freeze, secrets and VM machines:
  - Reading run files in the editor; `/runs` is served, nothing calls it.
  - Webhooks for a graph land on the weida broker once it speaks HTTP and
    are relayed to `/triggers`.
  - `freeze` (SIGSTOP / cgroup freezer) as a per-node hold policy.
  - Secrets through weida's wrapped-secret flow: one refreshable token per
    run, child tokens per service ordered through it, everything invalidated
    when the run ends. Nothing secret-shaped in a graph document or in settings.
  - Workspace nodes (checkout, btrfs snapshot) producing the `cwd` a job
    runs in; caches per runner under its state directory.
  - VM guests with their own runner; a runner drives its VM over ssh instead.
