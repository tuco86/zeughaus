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
bar holds every connected runner's tabs and groups one after the other,
without section headers, plus the editor's `Local` scratch graphs (no
runner) or `Not running` graphs (their runner is not connected). A runner's
part is its shared workspace: loose tabs and one level of coloured,
collapsible groups, each tab a split tree whose leaves are terminals
(section 9) or graphs, and order, groups, splits and ids come from the
runner. Tabs never move between runners. A tab or group of a runner other
than this machine's (the one its endpoint file names) carries its host's
glyph, the `device=` label the CI lists for it (laptop, desktop, media,
server) or a remote glyph without one (`zeughaus/src/app/hosts.rs`). The
bar's toolbar opens a shell, a graph or a group on this machine's runner,
and the choice beside each on another one.

After the runners the `Projects` section holds one locked group per project
(`zeughaus/src/app/projects.rs`, `compose`): every project the user opened
(saved as `projects` in `editor.toml`) and every one a runner has a project
group for (`GroupSnapshot::project`), unless the user closed its group and
no tab came since. The group is named after the repository, tinted by its
name, and its header carries a terminal glyph per work shell (bringing it
to the front, in its agent's colour while one works or waits there; a dim
one while there is none asks this machine's runner for one with
`OpenProject`), then one glyph per CI channel (dev, nightly, release, the
rest) in the colour of its newest pipeline, each opening that channel's
view as a tab of the group. Its tabs are the project's Board, its masters,
its plan sessions, its other runner tabs and the channel views opened, in
that order; the runner tabs stay their runners' and carry their host's
glyph. A collapsed group repeats the badge of a tab whose agent waits.
Ctrl+PageDown and Ctrl+PageUp walk all of it in the bar's order, a group's
work shells before its Board. In both bar placements tabs are
dragged into and out of groups (collapsed ones too), onto another tab's
content to split it, and a pane is dragged by its grip back into the bar to
become a tab (`zeughaus/src/workspace.rs`, `drop_allowed`). A tab dropped on
another tab lands before or after it by the half the pointer is over; the
bar's empty rest after the last runner is that runner's end, and a drop
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
`zeughaus-runner install desktop` writes, named after the window's `app_id`. macOS has no
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

**Back and Forward** (`zeughaus/src/history.rs`, `app/history.rs`). After
every message the editor compares what the window shows with the current
place of an undo tree and records a change as a visit: back to the parent
is a Back step, to a child a Forward step, anything else a new child, so a
place reached after a Back branches off and the old way forward stays. A
place is a runner's tab or a CI view (by CI, repository, channel, pipeline
and job, since the views' ids are per process) with the ticket popout over
it; a place left within 750 ms of an automatic visit was passed through
and is taken out. Back and Forward skip places that are gone, the tree
keeps 100 places (the oldest branch goes first) and travels in the restore
file of a restart. The thumb buttons, the titlebar's arrows and the palette
walk it; the History button draws it as a git-style graph.

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

**Four exchange kinds** ride one pooled QUIC connection, told apart by their
first frame. A *control* exchange per client carries the attach (hello,
topology, one head per terminal, so the first paint is one round trip), the
structural commands with correlated, deduplicated replies, and every later
snapshot. A *terminal* exchange per attached terminal is full duplex: input
up, deltas down. *Row fetches* and *resource fetches* (an image's pixels)
are short exchanges of their own so a scrollback page or an image cannot
block a keystroke. A delta is computed per subscriber
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

**Images.** Sixel and iTerm2 images are drawn; kitty graphics stay off,
because its placements, deletes and unicode placeholders are not what a row
carries. The controlling pane reports its cell in device pixels with every
resize (`TerminalCommand::Resize { size, cell }`), the runner gives
`wezterm-term` the grid's size in pixels from it, so images land on cells
and a child's `CSI 14 t`/`CSI 16 t` is answered; the PTY itself keeps a
size of 0 pixels. A row carries `ImageSpan`s: which image (its content
hash), which part of it in image pixels, over which cells, inset within
them. The runner remembers the 64 images rows were last built with; a
client fetches each image's RGBA pixels once (`ResourceFetch`/`Resource`,
at most 8192 px a side and 16 MiB), holds 32 per terminal, and
`iced_terminal` draws them as textures through its glyph pipeline, above
the cell backgrounds and below the selection and the text. Text written
over an image replaces it cell by cell, as in any terminal.

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
not an error: `tail_records` returns the whole records that hold the last
bytes by walking the headers, and `first_at` the time of the first one. The
times sit on the byte stream, not on lines; a line's time is derived from
the chunk that holds its first byte, so a step that prints its name and
finishes later is one line at the moment it began (`timed_lines` in
`zeughaus-ci/src/excerpt.rs`). A shell's WAL is `wal` in its shim
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
| deployment | A host is bootstrapped once by `deploy/bootstrap.sh <profile>` (or a `deploy/setup-*.sh` that ends in it) and then updates itself (section 14, **Releases and the updater**). The `desktop` profile (`zeughaus-runner install desktop`) puts the binaries in `~/.cargo/bin`, the systemd user unit `zeughaus-runner` (`KillMode=process`: the shims outlive the runner process; `reload` is `SIGUSR1`), and `~/.local/share/applications/net.doodleshnookie.Zeughaus.desktop` with its icon, which is how a Wayland compositor shows the editor's icon. Where `systemd-run --user --scope true` succeeds at start, the runner starts every shim in a transient scope of its own, so systemd-oomd, which kills whole cgroups, takes one terminal instead of the unit with every shell |
| self-update | `<state-dir>/update.toml` (`channel`, optional `url`) turns the updater of the runner and of the CI on (section 14, **Releases and the updater**); every host has one, on `dev`. It works in `<state-dir>/updates/<build>/` and keeps its trial state in `<state-dir>/updates/state.json`; `<state-dir>/install.toml` (`profile`, `bin_dir`, optional `feed_addr`) says which files of the release's `share/` it rewrites for a new build. Without `update.toml` nothing is checked or fetched |
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
  --all-targets -- -D warnings` and the unit tier (`cargo nextest run -p
  <crate>`) of the crates the change touches. The pipelines run the rest: a
  push the unit tier and the doctests on every platform, the nightly and
  a release tag both tiers (section 14).

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

**Project groups.** A plan session, a master and a work shell get a tab in
the runner's locked group of their repository's origin instead
(`OwnedPlacement::Project`, `GroupSnapshot::project`), named after the
origin's last segment and tinted by a hash of that name
(`zeughaus_mux::project_color`), made by the first run that needs it and
gone with its last tab; a project group never takes the last group slot,
which stays for `Triggered`, so without a slot such a run lands there.
Closing a tab of a locked group ends its run or session. A CI job's run is
detached: it is watched through the CI's transcript. A work shell comes from
the mux command `OpenProject { origin, branch }`: the runner starts
`zeughaus-runner work <origin> --branch <branch>`, which clones
`<work root>/<origin>` when it is missing (the work root is
`$ZEUGHAUS_WORK`, else `~/work`) and becomes `$SHELL` there.

**Projects.** `/projects` (`zeughaus_link::projects`, JSON) lists what the
runner's user can open: the repositories each forge of `[projects]` in
`<state-dir>/zeughaus.toml` lists with push access (`[[projects.forge]]`
with `kind = "forgejo"` and its `url`, or `kind = "github"`, and `token`,
the argv whose stdout is the token), a mirror of a repository on
`primary` left out, plus every checkout under the work root
(`zeughaus-runner/src/projects.rs`). A listing is kept 300 s unless the
request asks for a refresh; a forge that fails adds a line to `errors`.

**Agent status.** Every terminal the runner starts sees its id as
`ZEUGHAUS_TERMINAL`. The zeughaus omp plugin (`omp-plugin/src/agent-status.ts`)
writes the main agent's state there as the user variable `zeughaus_agent`
(`OSC 1337;SetUserVar`, base64 JSON `{"v":1,"state","role","repo","doc",
"detail","at","pid","host"}`), only from the TUI and only on a change. The
terminal engine reports user variable changes (`Session::on_user_var`) and
keeps their values, which a reattached session gets back from its WAL's
replay; the runner parses the value (`zeughaus_link::AgentStatus`) and sends
it with the terminal's activity entry (`ActivityEntry::agent`), on
`/activity` too. Every 2 s it drops the status of an agent on its own host
whose `pid` is gone, until that terminal reports a new one. The editor
shows the status as the tab's role glyph in the state's colour, raises a
pinned notice for an agent that waits for its user while nobody looks at
its terminal (resolved once it works again, ends or is looked at), and
lists agents on the CI page's Overview.

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
`machine` topic, which every editor shows as a notice.
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
| tuco-pc | runner: containers, Windows VM | `zeughaus` | `10.8.0.10:7444` | `tuco-pc` (default runner) |
| atik (macOS) | runner, launchd agent | `uebelacker` | `10.8.0.7:7443` | `atik` |
| wohnzimmer (Windows 10) | runner, boot task | `zeughaus` | `10.8.0.12:7443` | `wohnzimmer` (`os = "windows"`) |

**Jobs are scripts.** Every `.sh`/`.ps1` file in `.zeughaus-ci/` with a TOML header
between `# /// ci` and `# ///` is a job named after its file. The header
holds these keys:

- `on`: `push <glob>`, `push <glob> of <repo>`, `done <glob> of <repo>`,
  `tag <glob>`, or `nightly`;
- `needs`: exactly one of `on` and `needs` is set;
- `channels`: only with `needs`; the channels the job is added in (names from
  `channels.toml`, or `tags`, `nightly`, `upstream`); without it a job with `needs`
  runs in every channel that selects what it needs;
- `image` (a `.zeughaus-ci/<image>.Containerfile`), `machine` (a runner or
  a Windows VM of one, by name) or `platform` (a key of `ci.toml`
  `[platforms]`, exclusive with `machine`); none means the host of the
  default runner;
- `cache`, `context`, `when_busy`: ignored, with a warning from
  `zeughaus-ci check` (**Workspaces**; no runner is ever busy);
- `secrets`, `env`, `timeout_minutes`;
- `state` (at most 8 store names) and `state_save` (`success`, the
  default, or `always`; only with `state`): data the job keeps from one
  pipeline to the next (**Job state**).

Files without a header are helpers. `zeughaus-ci check` validates a folder and
`zeughaus-ci plan` shows what an event selects, both without a service
(`header.rs`, `pipeline.rs`). The selection is the jobs whose `on` matches the
event, plus every job whose `needs` are all selected and whose `channels`, when
it names any, include the event's channel.

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

The job's environment has `CI_PLATFORM` when it has a `platform`.

A name that is neither a runner nor a machine of one, or the wrong script
kind, fails the pipeline's setup like an invalid header.

**Events are files.** A webhook (`serve --hook <addr>`, HMAC-checked, Caddy on
sadala forwards `https://ci.doodleshnookie.net/hook/<repo>` to it),
`zeughaus-ci run` and the nightly round all write `ci/inbox/*.json`. The
scheduler moves each inbox file to
`ci/queue/<repo>.<kind>.<hash(ref)>.json`, overwriting what waits
there. A newer push to a ref therefore replaces the one queued behind a
running pipeline and never cancels the running one. Without `--hook` the
service has no HTTP listener and pipelines come from `run`, the nightly
round and manual runs (**Manual runs**, whose events are keyed by upload:
none replaces another). An upstream event's hash also covers the upstream
repository and branch, not its commit.

**The nightly round.** `ci.toml`'s `[nightly] at = "HH:MM"` (the CI host's
local time; without it there is no round) is when the scheduler queues a
`nightly` event of every repository's default branch, all with the same
`received`, so the queue holds them in repository name order. Nightly
pipelines run strictly one at a time across the whole CI: a queued nightly
waits while any nightly runs, whether the round, `zeughaus-ci run <repo>
nightly`, `ci_run` or a rerun queued it. A round missed while the CI was
down does not run, and a repository without nightly jobs leaves no record.
Records and queue files from before the round say `cron` and read as
nightlies.

**Following another repository.** A job header's `push <glob> of <repo>`
selects the job for an `upstream` event: a push to a matching branch of
`<repo>`, another repository of `ci.toml`. When the scheduler sets up the
pipeline of a push (`set_up`, `queue_followers`), and the pushed commit is
still its branch's head, it reads the `.zeughaus-ci/` of every other
repository's default branch from that repository's mirror, without a fetch,
and writes an `upstream` event to the inbox for each one in which the event
selects a job. The event names the follower's default branch and carries
the push (`upstream`: repository, branch, commit); its pipeline fetches
and runs the follower's head, and its jobs see `CI_UPSTREAM_REPO`,
`CI_UPSTREAM_REF` and `CI_UPSTREAM_SHA`. Only push events fan out, so
there are no chains; a repository without a mirror follows nothing until
its first pipeline. Several upstream pushes queued behind a running
follower pipeline collapse into one for the newest commit through the
queue key. An upstream pipeline counts toward the default branch's streak,
like a nightly. Grants take the same pattern; `ci.toml` and `zeughaus-ci
check` refuse one that names no repository.

`done <glob> of <repo>` follows the end of such a push's pipeline instead:
when a push pipeline of `<repo>` that ran jobs ends (succeeded, failed or
cancelled, `finish_pipelines`) and its commit is still the branch's head,
`queue_followers` runs again with `upstream.done` set to the pipeline's
number and one `UpstreamJob` per job (`job`, `platform`, `status`,
`duration_s`). The queue key adds `done\n`, so a queued push follower and
a queued done follower of the same branch stay apart. The follower's jobs
also see `CI_UPSTREAM_PIPELINE`, and the assignment carries the results as
a JSON array (`Assignment::upstream_results`), which the runner writes to
`upstream-results.json` in the run directory (in `vm/` for a machine,
copied into the guest with `inner.ps1`) and names in `CI_UPSTREAM_RESULTS`.

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
(`release` on `tag v*`, `nightly` on `nightly`, `dev` on `push main` in
zeughaus's own file). Without
a match, or without the file, a push lands in a channel named after its
branch, a tag in `tags`, a nightly in `nightly`, an upstream event in `upstream`, a manual run in the channel of the
default branch, and records from before channels
are grouped the same way. The channel is decided in setup and kept in the
record; an invalid file fails the setup like an invalid job header. `check`
lists the channels, `plan` starts with the one an event selects, and
`status` shows it per pipeline. Retention counts per channel (**Disk**).
`clean = true` on a channel marks the pipelines it gets `clean` (kept in the
record): the first of their jobs at a place starts its workspace from nothing (**Workspaces**).

**The enlist protocol** (`zeughaus-link/src/enlist.rs`, `ENLIST_MAJOR` 8) is
one long exchange on `/enlist` (req/rep) per runner. Frames are a 4-byte
little-endian length and a postcard body, at most `MAX_ENLIST_FRAME_BYTES`
(1 MiB), refused before the body is allocated.

- The runner opens with `Hello { major, name, offer, build, runs, artifacts }`:
  its `Offer` (`os`, `arch`, `cpus`, whether it has `containers`, its
  `machines`), the CI build it runs (0 for one built elsewhere; `ci_status`
  shows it), a `RunReport` for each run in its `assigned/` directory and the
  artifact sets it holds. The CI checks the
  TLS principal against the `fingerprint` of the `[runners.*]` entry that
  `name` names, the major, and that the runner's `os` is the host
  `ci.toml` gives it (`windows` or not), and answers `Refused { message }` or
  `Welcome { major, offsets }`, where each offset is how many bytes of that
  run's output the CI already holds.
- The runner then reports `Machine` (a machine's status), `Started`,
  `StartFailed`, `Output { run, offset, bytes }` (at most
  `MAX_OUTPUT_CHUNK`), `Finished { run, end }` and `Transfer { run,
  need, event }` (an input of the run `run` moving to this runner: `Started`,
  `Done { bytes }` or `Failed { message }`); the CI sends `Assign`, `Kill`,
  `Upload`, `Prefetch { run, consumer, need }`, `DropPrefetch { consumer }`
  and `Forget`. A run is named `<repo>.<number>.<job>` (`run_id`).
- `Assign` carries everything the run needs and the runner has not: the
  repository URL and the fetch token, the commit, the script, the place, the
  environment (`CI`, `CI_REPO`, `CI_REF`, `CI_SHA`, `CI_JOB`, `CI_PLATFORM`, ...), the
  granted secrets, the timeout, the inputs and how to check out
  the workspace (`Checkout`); the runner derives the workspace's name from
  the place (`Place::workspace`). The runner adds
  the paths and the CPU count (`CI_WORKSPACE`, `CI_OUTPUT`, `CI_INPUTS`,
  `CI_CPUS`, `CARGO_BUILD_JOBS`, `CI_SIBLINGS` for a manual run and
  `CI_STATE` for a job with `state`).
- Output is the run's `wal` byte for byte. The CI appends a chunk when its
  offset is the stored length, appends only the part beyond the length when
  the offset is below it (a resend after a reconnect), and ends the exchange
  when it is above: the runner reconnects and resumes from the new
  `Welcome`. Output of a run the CI did not assign to that runner is
  refused.

**Job state.** A job's `state = ["fuzz-corpus"]` names stores the CI keeps
for it, apart from workspaces, `clean` and runner budgets (`state.rs`).
A version of a store is a `SetManifest` under
`ci/state/sets/<repo>.<number>.<name>.set` (`number` is the pipeline that
saved it, the file's mtime when) whose files are zstd blobs in the store
`ci/state/blobs`; neither the artifact store's expiry nor
`artifact_store_gb` touches it, and the hourly sweep removes only blobs no
kept version names. The CI keeps the 3 highest numbers of each store; a
lower number saved later is kept but never becomes the newest. `Assign`
carries a `StateMount` per store: the newest version (`None` while there is
none) and whether the end saves it (`StateSave::Never` for a manual run,
else the header's `state_save`). The runner fills `<run>/state/<name>/`
before the job starts (`StateManifest`, `StateFetch` on `/artifacts`; the
blob cache keeps the version, so the next night moves only new files) and
points `CI_STATE` at `<run>/state` (`/ci/run/state` in a container). When
the run ends it saves each store unless it is a manual run, the run was
killed, or `success` and the job failed (`StateOffer`, `StateBlobs`, against
`state_max_mb` of `ci.toml`, 1024 by default), and reports a `StateResult`
per store in `RunEnd`. The CI only serves and accepts a store to the
runner assigned the run of a job that names it, and never accepts one from
a manual run. A failed save fails a job that passed; the note says what
became of each store (`state corpus saved 2.5 MB (started from #12)`). A
job on a Windows VM keeps no state. `ci_state`, `ci_state_get` (on
`/state`, editors' keys only) and `ci_state_delete` list, download and
delete versions.

The runner keeps the connection (`link.rs`): it redials after 1 s, doubling
to 30 s, starting over after a connection that lasted 60 s. It presents its
own `runner.pem` as the client identity, and the CI's URL in `enlist.toml`
pins the CI. Both sides log it: `[enlist] connected to <ci> as <name>`,
`refused: <message>`, `lost: <cause>`.

**A run on the runner** (`run.rs`). `Assign` is prepared on a thread of its
own, after the runner is no longer held (`zeughaus-runner hold`):

1. Fetch the mirror `ci/mirrors/<repo>.git` (the CI's own copy is a separate
   one), check the commit is there, pin it as `refs/ci/<n>`. A manual run has
   no commit on the forge: it downloads its bundles into `<run>/bundles/`
   instead (**Manual runs**).
2. Stage the inputs into `<run>/inputs/<need>/`, one thread per input:
   hard-linked from this runner's `ci/artifacts/<run>/` when the need ran
   here, else moved from what the runner fetched early (**Artifacts**), else
   fetched through the CI (manifest, then the files its blob cache lacks)
   and copied out of the cache, reported to the CI as a `Transfer`.
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

The launcher puts the place's persistent workspace
`ci/work/<repo>/<workspace>` (**Workspaces**) into the state the assignment's
`checkout` asks for and starts the job in it. `Clean` (the default) fetches,
checks the commit out and runs `git clean -ffdq`, which keeps ignored files
such as `target/`; `Wipe` runs `git clean -ffdxq`, which keeps nothing. Then
it starts the job:

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
  writable there. The VM's clock can run behind the host's while files
  written through the mounts carry the host's time, so the container has
  `CAP_SYS_TIME` and starts `clock.sh` instead of `inner.sh`: it creates a
  file in `/ci/run`, sets the clock to that file's stamp (the host's) plus
  5 ms, so a file written now never looks newer than the clock, and `exec`s
  `inner.sh`. A failure only warns and the job runs on.
- **Machine:** over ssh, from a git bundle, in `W:\work\<repo>\<workspace>`.

Host and container jobs time out inside `inner.sh`; a machine job on the
runner's own clock (`control.rs`), writes
`<run>/timeout` and kills the local ssh. `Kill` closes the run's terminal.

**Artifacts** go through `CI_OUTPUT`. An artifact set exists as a directory
on the runner that made it, and only as long as a job needs it. A runner
keeps a successful run's set and hard-links it as `CI_INPUTS/<need>/` for a
job of its own that needs it. For a job on another runner, the CI is the hub
and moves the set by content (`zeughaus-blobs`): a set is a `SetManifest`
(every directory, and every file by path, size, BLAKE3 root and executable
bit; symlinks followed), and its files travel as blobs named by their root.
The CI keeps a store of zstd-compressed blobs under `ci/blobs` and each
manifest it holds under `ci/sets/<run>.set` (3 days and `artifact_store_gb`
of `ci.toml`, default 2 GiB, swept at
start and hourly); every runner keeps a cache of raw blobs under `ci/blobs`
with its sets under `ci/blob-sets` (7 days and `blob_cache_gb` of
`enlist.toml`, default 8 GiB, swept the same way). A sweep removes the
blobs no kept set names and, over the budget, the oldest sets.

The consumer's runner asks for `Manifest { run }` on `/artifacts` (req/rep,
`ArtifactRequest`/`ArtifactReply`); the CI checks that it is assigned a job
that needs the run. When the CI holds no set of the run, it fills it once
for every consumer: it sends the producing runner `Upload { run, transfer }`,
which hashes the set and answers on an exchange of its own with `Offer {
run, transfer }` and the encoded manifest (or `Unavailable`, which fails the
fill at once; no offer for ten minutes fails it too). The CI answers with
the roots its store lacks, and the producer sends exactly those as `Blobs {
run, transfer }`, each a `BlobHead` frame and its zstd bytes (level 9). The
CI checks each blob's size and root as it decompresses it, stores it, and
records the set; then the consumer gets the manifest, asks for the roots its
cache lacks with `Fetch { run }` and gets them compressed as stored. It
checks and keeps them raw, copies the set out of the cache into its
destination (a reflink or clone where the filesystem has them, so a job
never changes the cache) and records the set in its cache. So a file
crosses a runner's uplink once, and not again while the CI holds it; a
consumer fetches nothing its cache holds. The `Transfer` it reports carries
the set's size (`bytes`) and the compressed bytes that crossed the wire for
it (`moved`), which `ci_timing` shows. Bodies are chunks: a `u32`
little-endian length in `1..=ARTIFACT_CHUNK` (1 MiB) and that many bytes,
ended by a length of 0. Every hop refuses a running total above
`MAX_ARTIFACT_BYTES`, and a body that ends without the 0 is a broken
transfer. A consumer is assigned only while the producing runner is
connected. A set does not wait for its consumer: when
a job that left one ends and a job that needs it is still pending on another
runner (waiting for its other needs, or for its runner), the CI sends that
runner `Prefetch { run, consumer, need }`. The runner fetches the set as
the consumer's input into `ci/prefetch/<consumer>/<need>/` on a thread of its
own and reports it as a `Transfer`; the check on `Manifest` then admits the
runner of any job that needs the set and has not ended, not only of one that
is assigned it. The consumer's staging claims the fetch once: it moves the
set into `inputs/`, waits for it if it is still going, and fetches again if
it failed. A job that ends without being assigned (skipped, cancelled or
failed) after a fetch began gets its runner a `DropPrefetch { consumer }`;
`Forget`, a failed staging and the start of a runner remove what is left.
The CI ignores transfer reports about a job that has ended, so a job that
ends before its program starts closes its open transfers itself, at its own
end and with the error `the job ended before the set arrived`; at start the
CI closes such transfers in stored records too. Otherwise editors would show
those sets as moving for good.
Once every job that needs a set has ended (succeeded, failed or skipped), and
at once for a set nobody needs, the CI sends the producing runner
`DropArtifacts { run }`; a runner's `Hello` lists the sets it holds, and the
CI drops each one no pending job needs, which covers a drop lost with a
connection or a restart of the CI. Nothing else reads a finished set.
Only configured runners may use `/artifacts`. `ManualBundle { run, index }` is
the one request the CI answers from its own disk: bundle `index` of a manual
run, to the runner that run is assigned to
(**Manual runs**).
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
per job of the default branch (push and nightly pipelines), the first
pipeline of its current run of failures, and the last pipeline in which
every deploy job (one without `on`) succeeded. `zeughaus-ci status` leads with
it (`griasdi main: windows red since #138 (79e9035, 17 h, 13 pipelines); last
publish #136`). A failed job in a streak that began in an earlier pipeline
posts `red since #<n>: <cause>`. The pipeline that turns a job red, and
every red pipeline once a job has been red for an hour, raise an alert: a
line in the journal and `RuntimeEvent::Alert` on the `ci` topic, which
every connected editor shows as a pinned notice. A push to the
default branch whose commit has no `.zeughaus-ci/` is recorded as a
`no-jobs` pipeline and posts an `error` on `zeughaus/pipeline`, naming a
leftover `.ci/`; on other refs that stays silent.

**Load.** When a job is handed to its runner, the scheduler records on it
and on every other job active on that runner (a VM job counts as on its
runner) the other's `<repo>#<n>/<job>` in `shared_with`; two runs overlap
exactly when one is assigned while the other is active. A finished job's
load (`JobView::load_text`) is its run time over its expected time, in
tenths (`3.4x expected`), and how many other jobs shared its runner (or
`runner not shared`). A finished pipeline is judged by its first failed
job, else its slowest against the expected time (`pipeline_load`):
`ci_status` adds it to the channel's line, `ci_pipeline` as `load:` and
per job with a `shared with:` line, and the desktop under the pipeline's
row.

**Waiting, skipping, failing.** A job whose needs succeeded is `Waiting`
while its runner is not connected (`waiting for runner <R>`), is `Skipped`
after the runner's `wait_minutes` without it (`runner <R> offline`; the
forge hears `success`, so an offline Mac never turns a branch red), and
waits while its workspace (one job at a time per repository, runner and
place) is in use or promised to an earlier job (**Workspaces**), or its
machine is in use. A container place
on a runner that offers none, or a machine it does not list, fails the job. A
job whose need's artifacts are on another runner waits for that runner like
for its own (`waiting for runner <R> (artifacts of <need>)`, skipped after its
`wait_minutes`).

**Workspaces** are chosen by place. A job's workspace on its runner is
`ci/work/<repo>/<ws>`, where `<ws>` is `Place::workspace()`: `host`,
`image-<image>` (the image name without its tag, so a changed
Containerfile keeps the tree) or `machine-<vm>`. Every job of one repository at
one place on one runner shares that workspace, whichever pipeline it
belongs to and whether it is a pipeline job or a manual run, and finds what
the one before left, `target/` included, as a local development checkout
does; the same image on another runner, or another place, is another
workspace. The order between jobs of a pipeline is the `needs` edge, which
the CI tab draws; jobs that share a place and do not need each other run one
after another, oldest first. The header keys `context` and `cache` still
parse and do nothing (`JobDef.ignored_keys`), and `zeughaus-ci check` prints
a warning naming the file and the key: a repository's jobs at one place
share one workspace, and a checkout keeps ignored files.

`workspace_wait` holds a job back by two rules. Another job of the repository
at the same place on the same runner that is active blocks it (`waiting for
<job> in workspace <ws>` when that job is in the same pipeline, else
`workspace <ws> in use by #<n>`). A sharing job that is not terminal and sorts
before this one by (pipeline number, job index) blocks it too (`waiting for
<job> in workspace <ws>`, else `workspace <ws> queued for #<n>`). Only older
jobs block, and a job's `needs` point to earlier indexes of the record, so no
two jobs wait for each other and a younger pipeline never holds back an older
one.

`Checkout` is `Clean` or `Wipe` (`checkout_of`, `build_assignment`). `Clean`,
the default, fetches, runs `git checkout --force` and then `git clean -ffd`:
untracked files go, ignored files such as `target/` stay. `Wipe` is the same
with `git clean -ffdx`: nothing stays. A job gets `Wipe` only in a pipeline
whose channel is `clean`, and only when no earlier job of that pipeline with
the same runner and workspace has started: the first use of a place in the
pipeline starts it from nothing, the rest build on it. A manual run is never
`clean`.

**Manual runs** run named jobs of the repository's pipeline from the working
trees of an editor's agent: uncommitted files and the sibling path
dependencies of its checkout included. The agent snapshots its
checkout and each dependency as a commit without a parent (the tree `git add
-A` sees) and bundles each under one ref of `refs/zci-manual/`. The CI's
`/manual` service (`manual.rs`, wire types in `zeughaus_link::manual`,
`MANUAL_PATH`) takes one exchange per run from an editor's key (a runner's is
refused): a `RunUpload` frame, then the bundles as raw bytes, answered
`RunReply::Accepted { repo, number }` or `Refused { message }`.

- The upload names the `origin` of the main checkout, the uploader (`actor`),
  the one-line `summary`, the `jobs` and the checkouts: directory name,
  commit and bundle size. `jobs` has 1 to `MAX_MANUAL_JOBS` (16) entries, each
  a plain name, none twice (`job <name> is named twice`); there are at most
  `MAX_MANUAL_DIRS` (8) checkouts, each bundle at most
  `MAX_MANUAL_BUNDLE_BYTES` (512 MiB). A name that is no plain name, a commit
  that is not 40 hex digits and a size over the limit are refused before a
  byte is stored.
- The repository is the `ci.toml` entry whose `url` names the same repository
  as `origin` (compared in lowercase, without scheme, user, trailing `/` and
  `.git`, an scp-style `host:path` as `host/path`); an origin that matches
  none is refused (`<origin> is no repository of ci.toml`) before a byte is
  stored.
- The bundles are stored as `ci/manual/<id>/<i>.bundle`, then `upload.json`;
  an event of kind `manual` with `ref` the repository's default branch, `sha`
  the main checkout's commit and `upload = <id>` goes to the inbox, and the
  exchange waits up to 30 s for the scheduler to write the pipeline's number
  to `ci/manual/<id>.number` (beside the directory, which goes at once with a
  pipeline that does not start). A manual run posts no commit status, raises
  no red alert and cannot be rerun (`a manual run cannot be rerun; start a
  new one`): its trees were the uploader's. Its upload and the mirror below go
  when its pipeline ends; directories and number files a week old are swept
  when the CI starts. Its events are keyed by upload
  (`<repo>.manual.<upload>`), so none replaces another.

`Scheduler::set_up_manual` makes the pipeline from the working tree, not from
the commit on the forge:

1. `sha` is the main checkout's commit and the pipeline is not `clean`. The
   repository must be in `ci.toml`.
2. The first bundle is fetched into the bare mirror
   `ci/mirrors/<repo>.manual.git` and `.zeughaus-ci/` is read at that commit,
   so the jobs are the working tree's (`no .zeughaus-ci/ in the working tree`
   when it has none).
3. The channel is the one a push to the repository's default branch selects
   (`dev` for zeughaus), the subject is the summary's first line followed by
   the job names in parentheses.
4. Only the named jobs are planned, in the folder's order. A name with no job
   is an error (`no job <name> in .zeughaus-ci/ of the working tree; its
   jobs are <names>`). A `needs` on a job that is not named is dropped, so
   `check-<p>` runs after `build-<p>` when both are named and alone when only
   it is, and the secrets are cleared: working-tree code gets none.
5. `place_job` places each job as for a push. A job that resolves to a
   Windows VM ends the pipeline as `error` (`manual runs run on a runner's
   host or in a container; <job> runs on the machine <vm>`): a VM's workspace
   is one bundle, copied into the guest.

An error there is recorded and reported like a failed setup. The jobs then
wait for their workspaces like the pipeline's own do (**Workspaces**): a
manual run takes its turn at a place among the pipelines' jobs, by number.

The assignment has no forge access and no secrets and carries an
`Upload { dirs }`. The runner (`run.rs`) skips the mirror, downloads
bundle `i` of each dir into `<run>/bundles/<i>.bundle` (`ManualBundle`) and
renders `launch.sh` (`launch.ps1` on Windows). The main checkout is the
place's workspace itself, so its paths are the pipeline's: enter the
workspace, `git init` if new, add `/.zci-siblings/` to `.git/info/exclude`,
`git fetch --prune` the bundle's `refs/zci-manual/*`, `git checkout --force
--detach <sha>` and `git clean -ffdq`. Every further checkout does the same in
`<workspace>/.zci-siblings/<dir>`. There is no `-x`, so ignored files such as
`target/` stay warm from the pipeline's builds to the manual run and back.
Then it removes the bundles, touches `.git/zci-last-used` (what the runner's
budget reads) and starts the job in the workspace: `CI_WORKSPACE` is the
workspace (`/work` in a container, which always mounts the workspace there)
and `CI_SIBLINGS` is `<workspace>/.zci-siblings` (`/work/.zci-siblings`), so
a job finds a sibling path dependency where its script links it. The
runner refuses an upload that has no checkouts, more than `MAX_MANUAL_DIRS`,
a name or commit that is not plain, or a machine place.

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
  not started is lost, and the CI assigns it again. `Forget` removes a run's
  file.

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
`/snapshot` (no outputs) and `/mux`. An editor's
key is under `<state-dir>/clients/`; a runner's is pinned by `ci.toml`, and
each service admits only the kind of peer it is for. `/ci` answers an
overview (every channel with its newest pipeline, and the machines: every
configured runner in name order as `kind = "runner"` with `offline` or
`online`, then each
machine the connected runners report as `kind = "windows-vm"`, each with the
jobs active on it as `MachineJob { repo, channel, number, job }`, and the
`[platforms]` of `ci.toml` as (platform, target) pairs), a channel's pipelines
newest first by pages, one pipeline, opens or closes a transcript, answers
`Log { repo, number, job, tail, timestamps }` with the last lines of a job's
stored output as plain text (at most 5000 lines and 3 MiB, read from the end
of its `wal`, `finished` once its `exit` exists; with `timestamps` every line
starts with `[+<seconds>s]` since the job's first output, by the time of the
record that holds the line's first byte, and lines with no text are left out),
answers `Timing { repo, number, job, top }` with the `top` (1 to 100) lines of
a job's output that the longest pauses follow, in output order, each with the
time it began after the first output and how long it stood (until the next
line began; the last one until the end of the job, `finished` of its record,
else now), and with the number of lines, the span, `finished` once the `exit`
exists and `truncated` when the 64 MiB it reads from the end of the `wal`
did not hold all of it (a window that opens inside the output drops its first
line, which may be the end of one that began before), answers
`Find { repo, sha }` with the newest
of a repository's last 500 pipelines, of any channel and a manual run too, whose
commit id starts with the 4 to 40 hex digits `sha` (`Pipeline`, empty when
none does), answers `Repo { origin, sha, jobs }` (for an agent in a
checkout: `origin` is its origin URL, at most 1024 bytes, matched against
the `url`s of `ci.toml` as a manual upload's is; `None` when no repository
has it, with the pending onboarding request of that origin as `pending`)
with the repository's name, forge, default branch and that branch's
streak (`green`, or red since when), its channels, the newest pipeline of
`sha`, the machines and the platforms, and with `jobs` also every job of its
`.zeughaus-ci/` read from the CI's mirror at `sha` when the mirror has it,
else at the default branch's head, each with the runner and place
`place_job` picks (or why none), its secrets and whether a manual run can
take it (not on a VM); `jobs_note` says why the list is empty (not fetched
yet, unreadable); with `setup` also the setup check (the hook's last
delivery from `ci/hooks/<repo>.json`, the bot's push access asked of the
forge, the secrets it names, the job secrets no grant releases) -- and
takes two actions, both answered `Accepted`:
`Rerun { repo, number }` writes the pipeline's event to the inbox again,
pinned to the recorded sha, with the actor `rerun of #<n>` (a manual run is
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
(`zeughaus/src/app/ci.rs`, the views in `ci/page.rs`) each URL of `ci` is a
link of its own. The status bar's chip opens the Overview over the workspace
(the chip says how many things need the user and how many work, coloured
by the worst; the palette's "CI / Open page" does the same): `Needs you`
and `Working` (agents that wait or work, as their terminals' activity
reports them, tickets awaiting review, failed channels, sessions failed in
the last day, running channels and live sessions; a terminal row's terminal
is docked read-only beside the list, and Enter or a click brings its tab
to the front), the projects (the local runner's `/projects` and every CI
repository, filtered by a search, each shown as a group with its Board on
Enter), and each CI's runners (device glyph, name, state, running jobs
that open their transcripts; under them the chosen runner's `/activity`,
`Reopen` for an archived run). A project's Board and its channel views are
tabs of its group in the bar. The Board is the project's lifecycle on one
screen: the channel rail (glyph in the tone of the newest pipeline, name,
the queued events as hollow pulsing dots, then the newest 12 pipelines as
dots that open the channel at that pipeline; a running one pulses, a held
one wears a ring in its holder's agent colour), then the columns
PROPOSALS, REVIEW, RUNNING and DONE (the newest 12) with two lanes
between them that are drawn only while a card passes through: PLANNING (an
open ticket a session holds the claim on, a ready or planning ticket, and
plan sessions without a ticket) and STARTING (a ticket whose master has not
started yet). RUNNING lists the tickets in progress, then a `queued N`
separator (`queued N, paused` while the repository's queue is paused) and
the approved tickets in the order masters take them; its count is the
running ones. A ready ticket that waits for a ticket it depends on stays in
PROPOSALS with an `after #n` chip; a ticket merged into one that is not
finished is left out, the including ticket's card names it. Cards show
number, title, age, `Runs on`, the tickets it includes and `plan v<n>`; a
card in a lane or a running one adds its holder's host glyph and its
agent's role and state, a running ticket the pipelines its master holds
(`waits on`, `fixing`, `holds`), and an action on its way
its verb. A lane card has a flowing bar and a breathing border, as has a
card an action is on; the page ticks every 50 ms while anything pulses.
`h`/`l` and Left/Right change the column, `j`/`k` and Up/Down the card,
Enter on a card of the lanes or a running one brings its session's (else
its claim holder's) tab to the front and on any other opens the ticket's
popout, and `a`, `r` and `p` approve, dismiss and release for planning
where the ticket offers it. The popout (`ci/ticket.rs`) lies over the
workspace under the history tree, the notices and the palette: the ticket's number,
title and state, its actions (`Edit` of an open or ready ticket, what it
waits for, the note of a session that ended without a plan), the tabs
`Proposal` and `Plan v<n>`, and a side column with its state, source,
`Runs on`, links both ways (added by number, removed), the tickets it
includes, its sessions and its claim's holder. A plan in review is drawn
in the sections `ci/review.rs` splits it into at its headings; each is
folded, deleted with its subsections or annotated, beside an overall note,
and `Refine` sends that review (`DocRefine`) while `Approve` waits until
the review is empty. Under the text every ticket, finished ones too, lists
its comments and a box for a new one (`DocComment`, its author the
editor's `<user>@<host>`). `Re-evaluate` of an open, ready, reviewed or
approved ticket asks the CI (`Reevaluate`, one at a time, the other
buttons free meanwhile); the stored suggestion shows under the actions,
a rewrite with `Edit with this text`. An agent's open ask about the
ticket (`ci/ask.rs`) is answered under the actions: its questions with
their options as pills, a box for an answer of the user's own and `Send`
(`AskAnswer`); the same form stands over the docked terminal of the
agent's row in Needs you, or alone for an ask whose agent has no row.
The CI keeps asks in memory under a 30 s lease that the omp plugin, which
posted the ask, renews by polling it (`zeughaus-runner ci ask` does the
same for a script); the
first answer, an editor's or the terminal's, wins. Escape or a press on the scrim closes it; `New
ticket` on the Board writes one in it. A card the CI moved to another
column carries an accent ring that fades over 2 s. A project no CI builds
gets the Board of `ci/onboard.rs`: the first CI's advice on it
(**Advice**): cards of work to choose and the onboarding they file in one
step with that CI. A channel lists its pipelines in a scrollable, newest on top
and separated by rules, one row each: a status glyph (codicons from the
bundled Nerd Font, so status never rests on colour alone), `#<n>` and the
commit's subject (the CI reads it from its mirror when the pipeline starts,
and once at start for records without one), the branch, tag or nightly, the
commit and the actor, the cause line of each failed job, for a failed
pipeline who is responsible (the holder's role glyph and `responsible:
<holder>`, `(holds #m)` when inherited; a press brings the holder's tab to
the front), the load of a
finished pipeline (**Load**), and the duration
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
pipelines. Up and Down (or `j` and `k`) move a cursor over the list in
front, Enter opens or closes the pipeline under it, `r` reruns it, Shift+`C`
cancels it and `l` opens the log of its first failed job (letters no widget
took), and Escape closes the Overview, when no palette or rename takes it
first. The
list is at most 1200 px wide. While the channel in front has something running, a
one-second clock moves its elapsed times. The cables between the cards are one
`iced_nodegraph_sdf` primitive under them; nothing pans, zooms, moves or
takes the wheel, so the list scrolls like any other. Pressing a job
opens its transcript in a tab of its own in the project's group, right
after the channel's tab (`ViewKey::Job`, a `JobRef` of channel view,
pipeline and job), or brings that tab to the front; its card is tinted
with the accent while the tab is open, and closing the tab closes the
transcript. The tab shows the channel as a link back, the job's status
and duration over the transcript. The CI creates a
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
after the CI restarts. The page's ids are the editor's own and no CI knows
them (`SurfaceRef::Ci` stays in the wire type, unused). An editor's link to
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
of `ci.toml`, led by the CI's own build, and each enlisted runner with the
build its hello named),
`ci_pipelines` (one channel's, or without `channel` every channel's, each
line then starting with its channel), `ci_pipeline` (a failed job adds its
`cause` and `excerpt`; a finished job its load, **Load**), `ci_log` (`CiRequest::Log`: the last lines of a
job's stored output, 5000 at most, and whether the job has finished;
`timestamps` starts every line with the seconds since the job's first
output; `pattern`, a regular expression of at most 1 KiB, keeps the lines
that match it, within `lines` when given, with `context` lines around each,
at most 20), `ci_timing`
(without `job`, from the pipeline's record alone: when it was created and how
long it took, and for each job when it was assigned, started and ended as
seconds after the creation, how long it staged (assigned to started) and ran,
and the inputs that moved to its runner, marked `(fetched early)` when one
was there before the job was assigned; with `job`, `CiRequest::Timing`: the
`top` lines, 15 unless told, that the longest pauses of its output follow),
`ci_wait` (one
pipeline, named by number or by the sha of its commit, or one job of it,
looked at every 2 s until it has settled; a sha that matches nothing yet is
waited for, and then followed by its number; at the end of the wait it
says `still running after <s> s; call ci_wait again with repo=.. number=..`
and names `zeughaus-runner ci wait <repo> <number>` for a background wait),
`ci_rerun` and `ci_run` (`CiRequest::Queue`: a push or nightly event of the
default branch, with the asker's `<user>@<host>` as its actor; both look
for the pipeline the CI numbers for it for up to 30 s and answer with its
number), `ci_cancel`, `ci_repo`, `run_jobs` and the job state tools
`ci_state`, `ci_state_get` and `ci_state_delete`; their answers are one
fact per line
in a fixed shape, and a CI that answers with an error, or not at all within
30 s, is the tool's error. No schema carries a `maximum`, because omp
refuses a call above it; the tools clamp instead and the descriptions name
the limits. A question that only reads is asked once more on a new
connection when the exchange broke off (a CI restart); one that changes
state is not.

`ci_repo` (`repo.rs`) reads the checkout at `path` (default: the working
directory): its top level, its `origin` with credentials cut, and `HEAD`,
and asks `CiRequest::Repo` with `jobs`. It answers the repository's name and
forge with the default branch's streak, the pipeline of `HEAD`, the
channels, one padded line per job (runner and place, `on` or `needs`,
secrets marked as absent in a manual run, `run_jobs cannot run it (Windows
VM)` for a job on a machine, `cannot be placed: <why>`), the runners and
machines and the platforms. Every tool that takes `repo` may omit it: the
working directory's checkout is then resolved the same way (without the
jobs) and its name kept for the session; `ci_wait` with neither `number`
nor `sha` waits for the sha of that checkout's `HEAD`. `zeughaus-runner
[--state-dir D] ci repo [--json] [<path>]` and `ci find [--json] <repo>
<sha>` are the same two exchanges for a script (the `zeughaus-ci` omp
plugin): `--json` prints `{"repo": RepoView, "text": ..}` or the
`PipelineView` (`null` when there is none); the exit code is 0 when
answered, 3 when the origin is no repository of `ci.toml`, 2 when no CI is
configured or the exchange failed, 1 for bad arguments or no checkout.
`ci wait <repo> <number|sha> [<job>]` is `ci_wait` for up to 4 h, for an
agent's background command: exit 0 once settled, 2 when it ran out or the
CI could not be asked.

`run_jobs` runs named jobs of the repository's pipeline from the working
tree it is called in (`jobs`, one to 16 names: the files of
`.zeughaus-ci/` without extension, e.g. `build-windows-amd64`; `path`; and
`wait_seconds`). `snapshot.rs` makes, for the checkout of the call and for
each Cargo path dependency beside it (the packages of
`cargo metadata` without a source whose manifest is outside the checkout;
one that is not in a git checkout with the same parent directory is
refused, because the runner lays the checkouts out side by side), one
commit without a parent: in a temporary index, `HEAD`'s tree, then `git add
-A`, so with every change and every untracked file the ignore rules do not
exclude. The checkout's own index, branch and refs are not
touched. The `summary` is the subject of the main checkout's `HEAD` (`git
log -1 --format=%s`), or `working tree` when it has no commit. Each commit
travels as a bundle of one ref, at most 512 MiB and at
most 8 checkouts, in one exchange on `/manual` (`RunUpload`, then the bundles
as raw bytes, answered with `RunReply`); the CI's refusal is the tool's
error. On `Accepted { repo, number }` the tool calls `ci_wait` for that
pipeline with what is left of `wait_seconds` (540; at most 600,
because the client's own limit for a call is 660 s), counted from the start
of the call, snapshot and upload included, and answers as `ci_wait` does,
including `still running after <s> s; call ci_wait again with repo=.. number=..`
for a run that is still going. The `origin` remote names the repository (the
CI matches it, so credentials in a remote's URL are cut before it is sent)
and `actor` is `<user>@<host>`.

**Disk.** The CI keeps the 200 newest pipeline records per channel, with
their transcripts (running ones never go), and no artifacts. A runner
deletes workspaces beyond its `budget_gb` (50) least recently used first,
through `podman unshare rm` because podman containers leave subordinate-uid
files (plain removal under Apple's `container`, whose virtiofs writes files
the user owns), removes image tags no pipeline uses (`podman rmi`,
`container image delete` on macOS), deletes an artifact set when the CI
drops it, and a run's `inputs/` when the run ends. Image removal and the
engine's prune collect blobs no tag references yet, which includes an image
that a `build` is loading, so a container job builds its image under
`zeughaus-runner image-lock <state-dir>/ci/images.lock -- <build>` (a shared
`flock`) and the removal takes that file exclusively; it skips while a build
holds it, and the end of the next container run prunes instead.

**Secrets** live in OpenBao (KV v2), by user, then by reader:
`zeughaus/zeughaus` is the CI's own document (`[secrets] path` in
`ci.toml`: forge tokens, webhook secrets, the Claude sign-in),
`zeughaus/<origin>/shell` and `zeughaus/shell` hold what a project's shells
and jobs get, `zeughaus/<origin>/omp` and `zeughaus/omp` what its omp
sessions get; `ci.toml` only names them. The CI, its hook and the CLI log in
with the AppRole `zeughaus` (`secrets.rs`), whose secret_id is sealed with
`systemd-creds` to the host and the CI user (`deploy/ci/openbao.sh`, run on
sadala, creates and rotates it); its policy reaches no document of the
owner's `hannes/`. The token stays in the process; the own document is read
at use time and kept for `cache_ttl_seconds`, never served stale, so a
value rotated in OpenBao takes effect without a restart. When OpenBao cannot
be reached, fetches, job starts and statuses fail with its message and the
hook answers 503, which the forge redelivers.

A job gets only the names its repository's `grants` release to the event,
through a hand-off (weida's, its decision 0032): the CI mints a job token
through the token role `zeughaus-job` with the policy `zeughaus-job-<repo>`
(read `zeughaus/<origin>/shell` and `zeughaus/shell`, written by
`openbao.sh` from `zeughaus-ci job-policies`), wrapped for 120 s, and sends
the wrapping token in `Assign` (`SecretHandoff`). The runner redeems it
before anything else of the run (a token already redeemed is an alarm and
the job does not start), reads the names, the project's value before the
global one, revokes the job token and hands the values to the job through
files; the CI revokes it by its accessor once the run started, failed to
start or ended. The persisted assignment never holds the wrapping token.
The runner masks each value of at least 4 bytes as `*` in the log it sends
the CI, which keeps every byte offset: records are held back until no
occurrence can still reach into them (`enlist/mask.rs`). Its own WAL keeps
the output unmasked, and a run a restarted runner recovers is delivered
unmasked. `zeughaus-ci secrets-check`, the repository setup lines and every
start of `serve` list each own secret and, read through a job token as a
job reads them, each granted one as present or missing.

A runner's shells and agent sessions get the zeughaus secrets of their
kind (`mux/secret_env.rs`, through `get-secret --json` with the runner
user's own OpenBao credentials): a project's work shell
`zeughaus/<origin>/shell`, then `zeughaus/shell`; an agent session
`zeughaus/<origin>/omp`, `zeughaus/omp`, then the two `shell` documents; a
pane's shell `zeughaus/shell`. What the terminal's profile or the runner's
own environment sets wins. A runner without credentials opens its
terminals without them and says so once.

**Releases and the updater.** zeughaus's own `.zeughaus-ci/` produces the
releases its hosts follow. Per platform one build job,
`build-<platform>`, builds the editor and the runner in a full build on its
own platform, with no cross compilation of Rust code, and packs
`zeughaus-<version>-<platform>.tar.gz` (`.zip` on
Windows) into `CI_OUTPUT`, with the workspace version, `LICENSE-*` and a
`share/` directory in a top directory `zeughaus-<version>-<platform>/`.
`share/` holds repository files under their repository paths, which `install`
and the refresh after an update read instead of the checkout (`stage_share`
in `build.sh`): on Linux `deploy/zeughaus-runner.service`,
`deploy/net.doodleshnookie.Zeughaus.desktop`,
`zeughaus/assets/icon/zeughaus.svg`, `deploy/runner/zeughaus-runner.service`,
`deploy/runner/ssh_config`, `deploy/ci/zeughaus-ci.service` and
`vm/win11/{vm.sh,autounattend.xml,setup.ps1,firstboot.ps1,toolchain.ps1,prepare.ps1}`;
on `darwin-arm64` `deploy/macos/net.doodleshnookie.zeughaus-runner.plist`; on
`windows-amd64` `deploy/windows/maintain.toml`. They are files of the archive
and not `include_str!`, because every crate is published and a package
cannot include files outside its directory. The build number
`CI_PIPELINE` is compiled in as `ZEUGHAUS_BUILD` by the build jobs, not by
`cargo_env`: it changes with every pipeline and would rebuild every unit
that reads it in the shared workspace. A binary made anywhere else has
build 0. Per platform one check job, `check-<platform>`, `needs` the build
job, so it runs in the workspace the build left, at the same place: it runs
`cargo clippy --workspace --all-targets -- -D warnings`, the tests (`cargo
nextest run --workspace`: the unit tier on a push or manual run, `--profile
ci` with the integration tier on the nightly and a tag) and `cargo test
--workspace --doc` (`check_platform` in `build.sh`; `windows.ps1` for the
Windows jobs), and on `linux-amd64` also `cargo fmt --check`. Every OS
therefore passes the gate. The build is a release build
and the check a dev one, so they share the workspace's cargo home and
checkouts but no compiled units: the order puts the archive out early (the
CI moves it towards `publish` while the checks run) and skips the check of a
platform whose build broke. Every command runs as a step that prints
`== <command>: <s> s` (`step` in `build.sh`, `Run` in `windows.ps1`), which
`ci_timing` reads. The toolchain is pinned in `rust-toolchain.toml` and the
image's `FROM` tag, so all places build with one compiler. The jobs of a
platform share their place and so their workspace. weida is a git
dependency pinned to a commit of its public GitHub mirror (`rev` in the root
`Cargo.toml`): cargo fetches it into the workspace's cargo home, no job needs
a secret for it, and a manual run builds the same weida as the pipeline.

|platform|build job and check job|where (`[platforms]` of the production `ci.toml`)|
|---|---|---|
| `linux-amd64` | `build-linux-amd64`, `check-linux-amd64` | `platform = "linux-amd64"`: tuco-pc, container `trixie` |
| `linux-arm64` | `build-linux-arm64`, `check-linux-arm64` | `platform = "linux-arm64"`: atik, the same Containerfile in an Apple `container` |
| `windows-amd64` | `build-windows-amd64`, `check-windows-amd64` | `machine = "wohnzimmer"`, the Windows runner's host (the `windows-amd64` platform stays on the `win11` VM, which griasdi's job needs) |
| `darwin-arm64` | `build-darwin-arm64`, `check-darwin-arm64` | `platform = "darwin-arm64"`: atik's host; the editor as `Zeughaus.app` (`deploy/macos-app.sh`) and the runner |
| `publish` | - | `machine = "sadala"`, needs all eight |

The Linux archives also hold `zeughaus-ci`, which the CI's updater installs.
`build.sh` (no header, so a helper) is sourced by the Unix build and check
jobs and by `publish.sh`; `windows.ps1` is the Windows jobs' helper (the
`Run` function, `CARGO_TARGET_DIR` under `<workspace>\.ci-cache` and
`Install-Nextest`). Every build job runs on a push to `main`, a `v*`
tag and the nightly round; a check job has no `on`, so it runs with the
build it needs. A push to any other branch starts nothing, and no channel is
named after a branch.

`publish.sh` needs an aarch64 host: it extracts `zeughaus-runner` from the
`linux-arm64` archive and runs `zeughaus-runner release bundle` over the
four archives, then `release verify`. The directory it makes holds the
archives, `manifest.json` (format 1: channel, build, version, commit, and
per platform file, size and SHA-256), `manifest.sig` (the raw Ed25519
signature of those exact bytes) and `SHA256SUMS`. Its channel follows the
event (push `dev`, nightly `nightly`, tag `release`; a tag must equal
`v<version>`). It is copied to `/var/www/zeughaus-updates/<channel>/<build>/`
(through a temporary name and a rename), `latest` is rewritten with the
build number, and all but the two newest builds of the channel go. A rerun of
an older pipeline leaves `latest` where it is. Caddy on sadala serves the
directory at `https://zeughaus.doodleshnookie.net/updates/` (`handle_path
/updates/*` with `file_server`, `latest` sent with `Cache-Control:
no-cache`). For nightly and tag events the bundle is also attached to a GitHub
release of `tuco86/zeughaus`: `nightly-<date>` as a prerelease (the seven
newest stay) for the nightly, `v<version>` with the version's CHANGELOG section
as its body for a tag, a prerelease when the version has a `-`. The two
secrets this needs are in the OpenBao document next to the others and
granted to `push main`, `nightly` and `tag v*`: `ZEUGHAUS_UPDATE_KEY` (64 hex
characters, the Ed25519 seed from `zeughaus-runner release keygen`) and
`ZEUGHAUS_GITHUB_TOKEN`. `publish.sh` reaches GitHub through a header file,
never through argv.

**What a runner or CI trusts** is its own binary: the public key that signs
releases is compiled in (`TRUSTED_KEYS` in `zeughaus-update/src/manifest.rs`),
so a server that answers cannot make a host install anything the CI's key did
not sign. The signature is over the manifest, which binds the build number,
the channel and the size and SHA-256 of every archive; an old release served
as a new one, another channel's and a tampered archive are refused. The one
download that is not signature-checked is `deploy/bootstrap.sh`'s first
one, which trusts HTTPS to the update host and the archive's `SHA256SUMS`
line.

**The updater** (the `zeughaus-update` crate, which `zeughaus-runner` and
`zeughaus-ci` both use; `Product::Runner` or `Product::Ci`) starts with the
process when `<state-dir>/update.toml` exists. Its keys are `channel` and
`url` (default `https://zeughaus.doodleshnookie.net/updates`); an unknown key
or a malformed file is logged as `[update] <path>: <error>` and means no
updater. One thread waits 10 s, then looks every 60 s at
`<url>/<channel>/latest` with `curl` (Windows 10 ships `curl.exe`). A number
above the binary's own build, and not the refused one, starts an attempt in
`<state-dir>/updates/<build>/`: manifest and signature, then the archive of
the host's platform (`linux-amd64`, `linux-arm64`, `darwin-arm64`,
`windows-amd64`; an Intel Mac has none and logs that it does not update),
unpacked with `tar -xf` on every platform (GNU tar and bsdtar detect gzip, and
Windows 10's `tar.exe` reads the `.zip`). Nothing outside `updates/` changes
before every check has passed, and the last check is a **preflight**: the
staged binary runs as `<binary> version` and its stdout must be exactly
`<version> build <n>` of the manifest within 30 s (polled every 100 ms, then
killed). A binary that cannot load or panics at once fails the attempt as
`the new <binary> does not start: <status and the first 200 bytes of stderr,
or timed out after 30 s>` and replaces nothing. The editor is not preflighted.

Then the binary file is replaced, together with the editor installed beside
it for the runner: `zeughaus` in the runner's directory on Linux,
`zeughaus.exe` on Windows, `~/Applications/Zeughaus.app` on macOS (the bundle
written as `.new`); where there is none, only the runner is replaced, and if
one of the writes fails nothing is replaced. The CI replaces `zeughaus-ci`
only. The replaced file stays as `<target>.previous` (`Zeughaus.app.previous`
for the bundle): on Unix the old file is hard-linked (else copied) to
`.previous` before the rename of the new copy over the target; on Windows,
where a running executable cannot be overwritten but can be renamed, the
target is renamed to `.previous` and the new file to the target, and a
failure of the second rename puts the first back. Editors are then sent
`SIGUSR1`, which reopens them as they were (Linux: every process whose
`/proc/<pid>/exe` is that file, or `(deleted)`, and whose `SigCgt` shows the
handler; macOS: the pids `pgrep -f <app>/Contents/MacOS/zeughaus` finds;
Windows: none, a running editor keeps its build until it is reopened and
`[update] running editors keep build <old> until they are reopened` is
logged). The runner restarts itself through the same path as its own
`SIGUSR1` (section 9) on Unix. On Windows the runner updates only while it is
idle (not held, no live run: the predicate the maintenance tasks use) because
a restart ends every terminal and run; it saves the graphs and the workspace, logs
`[runner] restarting` and exits with 0, and the boot task's `runner.cmd` loop
starts the new binary about 5 s later. The CI's restart is an `exec` of its
own path with the same arguments (`[ci] restarting`); its listeners are
close-on-exec and its state is on disk, so the new image binds the same
ports, and an `exec` that fails is logged and the CI carries on. Every other
directory under `updates/` is then removed, except the installed and the
previous build's (their `staging/` is what the refresh reads); files in
`updates/` are never removed. A failed attempt is logged as `[update] build
<n>: <error>`, removed, and not retried by that process.

**Trial and rollback.** `<state-dir>/updates/state.json` (written by tmp and
rename) holds `installed`, `previous`, `starts`, `confirmed`, `refreshed` and
`refused`; unknown fields are ignored so it can grow. After an install and
before the restart it records `installed = <new>`, `previous = <own>`,
`starts = 0`, `confirmed = false`. At every start (`boot`, before serving) a
binary whose build is the installed, unconfirmed one counts a start; a
build 0, a missing file, another build or a confirmed one counts nothing. A
thread started with the updater confirms the installed build after 120 s of
running (`[update] build <n> confirmed`), also where `update.toml` is absent.
The 4th start of an unconfirmed build is the rollback: each target's
`.previous` is put back (Unix by rename; Windows by renaming the target to
`.failed`, the previous to the target and removing `.failed`; bundles the
same with directories), the state becomes `refused = <n>`, `installed =
<m>`, `previous = 0`, `confirmed = true`, `starts = 0`, `[update] build <n>
did not come up 3 times; back to build <m>` is logged and the old binary is
`exec`ed with the same arguments (Windows: the process exits and `runner.cmd`
starts it; a missing `.previous` logs that there is no previous build and
carries on). Starts are counted across the supervisors' restarts (systemd's
`Restart=`, launchd's `KeepAlive`, the `runner.cmd` loop); at a spacing of
about 3 to 5 s the 3 starts stay under systemd's default `StartLimitBurst`.
A refused build is not installed again; a newer one is tried normally.

`zeughaus-runner [--state-dir D] update status|rollback` and `zeughaus-ci
[--state-dir D] update status|rollback` read and undo the state. `status`
prints `following <channel> at <url>` (or `not following a channel (no
update.toml)`), `running build <own> (<version>)`, `installed <n>, previous
<m>, confirmed|unconfirmed, <k> starts` and `refused <n|->`. `rollback`
restores the `.previous` files and records it as the automatic rollback does,
but does not restart: it answers `rolled back to build <m>; restart the
service to run it: <hint>`, the hint chosen by the `profile` in `install.toml`
(`systemctl --user reload zeughaus-runner` for desktop, `sudo systemctl
--user -M zeughaus@ reload zeughaus-runner` for service, `launchctl kill
SIGUSR1 gui/$(id -u)/net.doodleshnookie.zeughaus-runner` for macos,
`schtasks /End /TN zeughaus-runner && schtasks /Run /TN zeughaus-runner` for
windows, `sudo systemctl --user -M zeughaus-ci@ restart zeughaus-ci` for ci);
without a `.previous` it fails with `no previous build to go back to`. Both
binaries also answer `version` with `<version> build <n>` without touching
the state lock, which is what the preflight runs.

**Install profiles and refresh.** `zeughaus-runner install
<desktop|macos|service|windows>` and `zeughaus-ci install ci` (`--channel`,
`--from`, `--bin-dir`, `--feed-addr`, `--no-service`; the details are in
`AGENTS.md`) write a host's binaries with the updater's own staged rename
(`install_binary`, `install_bundle`), the files of the archive's `share/` with
`@BINDIR@`, `@FEED_ADDR@` and `@LOGS@` replaced, and `update.toml` and
`install.toml`; `write_if_changed` in `zeughaus_update::host` makes a
repeated run write nothing. Service control (`systemctl --user` enable and
reload or start, `launchctl`, SIGUSR1 to editors) belongs to `install`
only. For every newly installed build whose first start finds `install.toml`,
`state.installed` equal to its own build and `state.refreshed` different, the
updater thread calls the product's refresh hook once with
`<state-dir>/updates/<build>/staging/zeughaus-<version>-<platform>` and then
records `refreshed = <build>`: it rewrites that profile's files (units,
desktop entry, icon, VM scripts, `ssh_config`, `maintain.toml`) and, when a
unit changed, runs `systemctl --user daemon-reload` and logs `[install] unit
changed; it applies at the next start of the service`. The binaries and the
service are left alone: the process is already the new build.

**Every host follows `dev`**, the CI's own hosts included (sadala: runner and
CI; the `tuco-pc` service runner; `tuco-pc-hannes`, atik and wohnzimmer
too), and those hosts own their binaries (`/var/lib/zeughaus/bin`,
`/var/lib/zeughaus-ci/bin`), so an update needs no root. A broken `dev` build
could in principle take down the CI that would publish its fix; the safety
net is the preflight (a build that does not run is never installed), the
trial (a build that does not stay up is replaced by the previous one after 3
starts and refused until a newer one is published) and `update rollback`
by hand. `release bundle` and `release verify` exist on every platform.

**Tickets.** The CI also stores the work an agent workflow passes around as
tickets: one number from proposal through plan to execution.
`zeughaus-ci/src/docs.rs` keeps each as the `DocView` the editor shows, in
`<state-dir>/ci/docs/<repo>/<n>.json`, with its proposal (`body`), its plan
(`plan_text`, `revision`, and the `planner`: the runner and omp session
file that wrote it), its links (`depends on`, `related`) and the tickets its
plan includes (`merged`), and the last number in `last`, so a deleted
number is never given out again. The state machine (`docs::allowed`) is
the user's contract: `open` -> `ready` (released for planning) or
`dismissed`, `ready` -> `open` or `dismissed`, `review` -> `approved`,
`open` (the plan discarded) or `dismissed`, `approved` -> `review` or
`dismissed`, `in_progress` -> `done` or `approved`. The CI moves the rest:
`ready` -> `planning` when it opens a plan session, `planning` -> `review`
when `PlanWrite` stores a plan (`ci plan-write`, the plugin's
`submit_plan`), or back with a note when the session ends without one;
`review` -> `ready` on a `DocRefine`; `approved` -> `in_progress` when a
master starts. A plan's `Includes:` line makes those tickets `merged` into
it; they close with it, and go back to `open` when it is dismissed or its
plan discarded. A link that would close a dependency cycle is refused. At
start the CI rewrites documents stored before tickets, and texts from
before Markdown tickets (`docs::migrate`).

A ticket in any state takes comments (`comments`, at most 100 of 8 KiB,
each with its author and time); they go into its next plan session's
brief and a resumed Refine, and lists leave them out. `Reevaluate`
(`reevaluate.rs`) asks the `[advice]` model, with the CI's Claude
sign-in and outside the documents' lock, what to do with an open, ready,
reviewed or approved ticket: from its text, plan, comments and links, the
repository's other unfinished tickets and the newest 30 commits of its
default branch in the CI's mirror, it answers keep, dismiss, merge (into
another unfinished ticket), plan or rewrite (a whole new ticket text),
with a reason. The answer is the ticket's `suggestion` until its state,
text or plan changes; one that arrives after the ticket moved on is
dropped. `/ci` requests are answered each in a task of their own, so a
model call, which may take 90 s, holds up no other question.

A ticket's text is Markdown (`zeughaus_link::ticket_text`): its first line
is `# <title>` and its link lines (`Depends on:`, alias `Blocked by:`, and
`Related:`, among the 20 lines under the title and outside fenced code) name
`#12` in its own repository or `#weida-12` in another. `create` and `edit`
read title and links from it; a link may cross repositories (`DocLink.repo`)
and the cycle check walks every repository's tickets. Answers fill
`waits_for`, the `depends on` targets not finished yet, which the dispatcher
and the Board read; it is never stored. A plan carries the work of another
repository, or a later ticket of its own, in sections `## <repo> - <title>
[#label]` (`plan_sections`) whose link lines may also name `#self` (the
plan's ticket) and other sections' labels. `PlanWrite` refuses a plan whose
sections do not resolve. Approving it (or its arrival, when pre-approved)
files each section not yet in `filed` as a `ready` ticket of its repository,
its references written relative to that repository, plus a `Related` link
to the plan's ticket; a section that cannot be filed keeps the ticket in
review with the reason. The master's plan leaves the filed sections out
(`without_sections`).

The `/ci` server, the dispatcher and the scheduler serialize every mutation
behind one lock and announce each on `/events` as `CiDocs`.

**Session requests.** A job asks for an agent session by leaving
`agent-session.md` in `CI_OUTPUT`, whatever its exit status. The runner
reads it in `Executor::finish`, before the output is kept or dropped (a
regular UTF-8 file of at most `MAX_SESSION_REQUEST_BYTES`, 32 KiB, so that
`Hello`, which carries every ended run the CI has not forgotten, fits its
frame; a symlink is refused), and carries its text, or why it was refused,
in `RunEnd::session_request`; a Windows VM job copies that one file back
from the guest after a failure too. `Scheduler::finish_job` hands it to
`docs::file_request`, which files it as an approved ticket with source
`job.<job>`, titled and placed by the same header parsers `ci plan-write`
uses (`zeughaus-link::ci`), when the repository's `agent_sessions` is set,
and files nothing while a ticket of that job in review, approved or in
progress has the same plan. A manual run or a cancelled pipeline files none. What
became of the request is a fragment of the job's note.

**Nightly repair.** A nightly of a repository with `nightly_repair` that
fails (a job failed, or the pipeline could not be set up) becomes a round
of tickets that run through (`zeughaus-ci/src/repair.rs`, from
`Scheduler::finish_pipelines` and the setup's error path): one approved
ticket per group of failed jobs, then one that depends on all of them,
carries no `Runs on` and queues the nightly again with `ci_run` without
waiting for it. A group is the jobs that need the same `Runs on`:
`os=<os>,arch=<arch>` of where they ran (a container is Linux, a machine
Windows, on their runner's arch) when a connected session host has those
labels, else `runner=<runner>` when their runner hosts sessions, else none.
Jobs whose cause line is the same (bracketed parts and path separators
aside) under several selectors are one group instead, placed by the first
of them. Each plan carries the failed jobs' notes, causes and excerpts and
the steps: `ci_log`, reproduce here or with `run_jobs`, fix, gate, push,
`plan_done`. The tickets are the only state: their sources are
`nightly.<n>` and `nightly.<n>.rerun`, no round is filed while one of them
is unfinished, and none after `repair::MAX_ROUNDS` (3) reruns created since
the newest nightly that succeeded, which raises an alert instead.

**Onboarding.** A third kind, `onboarding`, is a project's request to join:
an `OnboardSpec` (its origin, made the canonical url of `[onboarding] api`'s
Forgejo, its default branch, grants and host prerequisites) under the name
it asks for, `open` -> `approved` or `rejected`. While it is open or
approved the name has a document directory without being in `ci.toml`, so
tickets can be stored against it and the overview lists it with
`onboarding: Some(n)`, which gives it a Board; plans wait. The
onboarding worker (`zeughaus-ci/src/onboard.rs`, a thread every 2 s) takes
each approved request: secrets present, bot collaborator, webhook secret
(generated and patched into OpenBao, or reused), hook created or updated,
`ci/onboarded/<name>.toml` written and the configuration reloaded, first
push event queued. Every step may run again; a failure puts the request
back to `open` with the step and the reason as its note.
`CiRequest::Onboard` (`docs::onboard`) files a request already approved
together with its tickets in one step, as the owner's Board does: the
tickets `ready` and released, linked `depends on` by their places in the
request (a cycle or a place out of range refuses the whole request), the
request last, so the worker never takes one whose tickets are missing.
`ready()` and `queue()` see only repositories of `ci.toml`, so those
tickets wait for the join and then go through planning and the queue in
the order their links allow. A ticket's `auto_approve` makes `write_plan`
approve its plan as it arrives instead of putting it up for review.

**Claude sign-in.** `[claude]` of `ci.toml` (`zeughaus-ci/src/claude.rs`)
makes the CI the one holder of the owner's Claude subscription: omp on the
workstations and the CI's own requests use the access token it hands out
over `/ci`, and nothing signs in per machine. It is Claude Code's
public OAuth client (authorization code with PKCE S256, the scopes Claude
Code asks for), whose only redirect is `http://localhost:54545/callback`
(`zeughaus_link::CLAUDE_CALLBACK`), so the sign-in runs where the browser
is: `zeughaus-runner claude login` (`zeughaus-runner/src/claude.rs`) binds
that port on IPv4 and IPv6, asks the CI over `/ci` for `ClaudeStart` (the
CI keeps a state and verifier for 10 minutes and answers the claude.ai
address), opens the browser, takes the first `GET /callback` and sends its
code and state as `ClaudeFinish`, which the CI exchanges at
`api.anthropic.com/v1/oauth/token`; the browser tab then says how it went.
The request is an editor's (the state directory's `client.pem`), so only
the owner's machines can sign the CI in. The account of the answer (or of
the `claude_cli/bootstrap` lookup) must be `account`; then the refresh
token, the account and the time go into the CI's own document by merge
patch. A thread looks every minute: two hours before the access token
expires it refreshes (one at a time, under the state's lock) and writes the
rotated refresh token and the new access token with its expiry back at
once; a write that fails is kept in memory and retried the next minute. A
start reads the access token too and goes on with it, so a restart (every
update is one) renews nothing. A refresh revokes the previous access token,
so no machine keeps a copy: `zeughaus-runner claude token` asks the CI
(`ClaudeToken`, an editor's request like `ClaudeStart`) and prints the
current token, which omp's `models.yml` runs as its `apiKey` command. omp
caches it and runs the command again after a 401, so a renewal costs one
retried request. An `invalid_grant` ends the
sign-in (the stored refresh token is emptied), which happens about 30
days after it was made. `ClaudeView` in the overview carries the account,
when the sign-in ends, the token's expiry and the last error; the editor's
Overview lists it under Needs you while the CI is signed out, three days
before the sign-in ends, or while renewing fails, and a press runs
`zeughaus-runner claude login` beside the editor and reports the result as
a notice; omp's plugin offers `/zeughaus claude [login]`.

**Advice.** `[advice]` of `ci.toml` (a `model`, by default
`claude-haiku-5-5`, and the `api` base) is the language model the CI asks
with the Claude sign-in about a project that has not joined
(`zeughaus-ci/src/advice.rs`). The
editor asks its local runner on `/projects` for the facts
(`ProjectsRequest::facts`, `zeughaus-runner/src/projects/facts.rs`: the
skills `omp skill list --json` finds there, its listing, the work checkout
at `HEAD` with its top level, CI folders, manifests, README, remotes and
last commit, or else what the forge's API says, with push mirrors and a
same-named repository on a Forgejo) and sends them in `CiRequest::Advise`.
The CI adds what it knows of itself (its Forgejo, platforms, runners with
labels, repositories) and asks the Messages API through curl with the token
on stdin (a bearer token, the Claude Code and OAuth betas, and Claude
Code's identity as the first system block, without which a subscription
token is refused) for a small plan document in Markdown: a `# ` title and
the text under it (what the project is, where it stands), then 2 to 6
`## ` sections in the order they are to be done, a heading ending in
`(optional)` for work the owner may skip, skills named as `skill://<name>`.
`zeughaus_link::parse_plan` takes it apart (headings inside fenced code do
not count, `###` stays in its section) into `AdviceView::title`,
`summary` and `cards` (`Card`: heading, text, chosen). The Board renders
the summary and each card's text as Markdown; `Onboard` files one ticket
per chosen card with its text as the proposal, each depending on the one
before, named after the origin's last segment and the listed default
branch, without grants (a secret is granted later in
`ci/onboarded/<name>.toml`). The answer is kept under
`ci/advice/<sha256 of the origin>.json` with the SHA-256 of the request, so
the same facts against the same configuration are answered from disk
unless `refresh`. `AdviceView::refusal` (already in `ci.toml`, not on the
Forgejo, no `[onboarding]`) is the CI's own and is filled even without
`[advice]` and `[claude]`.

**Configuration reload.** Every service holds `config::Shared` and takes
one snapshot of the configuration per request or loop turn (the scheduler
per tick). `CiConfig::load` merges `ci/onboarded/*.toml` into `[repos]`
(a name or url that repeats one is refused). `Shared::reload`, on SIGHUP
or after an onboarding, swaps the snapshot when only `[repos]` changed and
refuses anything else, which still takes a restart.

**Agent sessions.** The dispatcher runs every 2 s. First every ready ticket,
by release, whose `depends on` tickets are finished and which nobody holds
or plans gets a plan session: on the runner of its `planner` when it has
one (it waits while that runner is away; the session resumes the omp
session file with the review attached), else on the first session host its
first `Runs on` selector matches, else the first. Then the approved
tickets of every repository whose queue is not paused, by approval and
again only once their dependencies are finished: a ticket's `Runs on` is
one label selector per machine (`zeughaus-link::selector`), the first
placed on a connected runner with `sessions = true` that hosts no live master
and is not cooling down after a failed start (the runner the ticket's master
last ran on, `placement[0]`, comes first and may take it back while cooling
down: it holds the ticket's clone and omp session), the others on distinct
connected runners with `ssh`, reserved for that master. A runner's labels are
`ci.toml`'s plus `runner=`, `os=` and `arch=` from its offer. The CI sends
the host `ToRunner::OpenSession` with a `SessionSpec`; the runner's agents
(`zeughaus-runner/src/agents.rs`, Unix) open the session in a terminal of
its mux and answer `SessionOpened` or `SessionFailed`, and a watcher per
session reports `SessionEnded` from the terminal's end, which the mux
remembers for closed terminals. A runner keeps its sessions in
`<state-dir>/sessions.json` and reports them in `Hello`, so a reconnect
settles sessions that ended meanwhile. A plan session without a ticket is
opened on request (`CiRequest::OpenSession`); plan sessions never count as
busy. The registry is `<state-dir>/ci/sessions.json`, every change
announced as `CiSessions`.

**Masters resume.** A master runs omp with the ticket's own session
directory (`<state-dir>/agents/<repo>/master-<n>`) and records the ticket
in the clone's `.git/zeughaus-master` before each start. It resumes the
newest session there whenever there is one (`start_of`): in a clean clone
after a fast-forward to the default branch, in a clone that holds changes
or unpushed commits only when the record names its own ticket. Work of
another ticket, work nobody recorded, or work of this ticket whose session
is gone refuses the master before its terminal opens (`SessionFailed`
with the reason). Inside one session the `master` process starts omp again,
resuming, while the ticket is still `in_progress`; three ends within a
minute of the start end the master. The CI settles every end of a master
in `docs::master_ended`: a refusal puts the ticket back into the queue
without its placement and with the reason as its note, and alerts every
editor; a master killed or lost with its runner goes back into the queue
with its placement, so its runner takes it again and resumes; a master the
user stopped (`close`) or that exited by itself leaves the ticket in
progress with a note, and the latter alerts. The Board shows a ticket's
note on its card.

**Reports.** A master ends with `plan_done` and its final report
(`CiRequest::DocDone`, `docs::finish`): the ticket becomes `done` and keeps
the report as `DocView.report`, a `TicketReport` of at most
`MAX_REPORT_BYTES` (64 KiB), whose text lists leave empty. A runner records
the repository and ticket of each master it holds; when one ends, its
`SessionEnded` (or the `SessionReport` of a later `Hello`) carries the last
assistant text of the newest omp session file of the ticket's masters, and
`master_ended` keeps it, marked `from_transcript`, only for a master that
stopped with its ticket still in progress. A new plan revision or a
discarded plan drops the report. `doc_read` shows it after the plan, the
ticket popout as a `Report` tab it opens on.

**Runners by name.** A runner's `Hello` carries its endpoint and the
addresses it answers at; the overview lists each runner's fingerprint,
labels and addresses, and the editor dials a runner only a CI announces as
`weida://<fp>@<name>.runner.zeughaus:<port>/`, which its own
`weida::Resolver` (`zeughaus/src/directory.rs`) answers from those
addresses.

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
- Jobs of the graph (`job.run`), decided but not built; CI (section 14)
  has its own webhook intake, secrets and VM machines:
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
