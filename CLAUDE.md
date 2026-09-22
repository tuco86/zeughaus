# Claude Code Instructions for Zeughaus

## Project Overview

Zeughaus is a visual dataflow workbench built on iced and iced_nodegraph. A
graph of nodes -- transforms, screen capture, SQLite schemas, LLM
conversations, Keras models, recordings -- is edited collaboratively and
executed by a headless runner that also multiplexes terminals for its editors.

**Architecture**: `DESIGN.md` is the one architecture document. Read it
before changing how the processes talk to each other, how values travel, or
how a node is configured. Everything above its "Not built" section describes
the code as it is.

**Status**: working. Editor and runner run against a local SpacetimeDB;
collaborative editing, live values, video feeds, subgraphs, drawn database
schemas, Keras export, recordings and terminal panes are in use. The browser
build of the editor compiles and edits locally (no store sync yet).

## Workspace Structure

```
zeughaus/              # binary: the iced editor; edits the graph and views results, never executes
zeughaus-runner/       # binary: headless process that executes the graph and owns the terminals
zeughaus-core/         # Ty/Typed/Value, pins, ExecutableNode/DomainPlugin, settings, scalar wire encoding, store row types
zeughaus-runtime/      # GraphExecutor: topology, node instances, edge cache, dirty set, async work, node errors
zeughaus-sync/         # SpacetimeDB client shared by both binaries: generated bindings, Store, Session, reducers
zeughaus-link/         # the runner<->editor protocol over weida (feed, events, snapshot, triggers, paths) and the credentials both ends present
zeughaus-mux/          # terminal mux wire model: stable ids, workspace topology, rows/deltas, bounded codec, client-side TerminalView (pure, wasm too)
zeughaus-terminal/     # the runner's terminal engine: PTYs over portable-pty and a pinned wezterm-term (native only)
zeughaus-module/       # SpacetimeDB server module (excluded from the native workspace; built by `spacetime build`)
zeughaus-transform/    # plugin: 35 math/logic/string/trig nodes, constants, Display
zeughaus-flow/         # plugin: Hold (event -> state), Button (manual event), Timer (the clock a source needs), All (fan-in)
zeughaus-graph/        # plugin: Subgraph container and its Input/Output boundary nodes
zeughaus-ml/           # plugin: Keras layers as nodes -> exportable functional-API code
zeughaus-llm/          # plugin: Conversation nodes against an LM Studio endpoint
zeughaus-capture/      # plugin: Screen Capture (xdg-desktop-portal on Wayland, scrap otherwise)
zeughaus-db/           # plugin: Database container, Table, Insert, Query, SQL; schemas drawn in the graph (SQLite)
zeughaus-record/       # plugin: Recorder (frames + values to disk) and Player (the same dataset as a source)
zeughaus-job/          # plugin: Job (a process in a runner-owned terminal, log per run under the state dir) and the ProcessHost trait the runner implements
iced_terminal/         # the terminal widget: one wgpu primitive per pane, bundled ComicShannsMono Nerd Font
iced_tabs/             # the tab bar the workspace shell uses
vm/win11/              # scripts: headless Windows 11 guest under QEMU/KVM, the reference for a VM-hosted runner (not wired)
```

Sibling checkouts this workspace depends on by path: `../iced_nodegraph`
(the node graph widget) and `../weida` (the QUIC transport).

## Running it

Three processes, in this order:

```
spacetime start                 # store, 127.0.0.1:3000
cargo run -p zeughaus-runner    # executes the graph and owns the terminals
cargo run -p zeughaus           # editor; start as many as you like
```

Publish the module once per schema change, and regenerate the checked-in
bindings:

```
spacetime publish --server local zeughaus --module-path zeughaus-module
spacetime generate --lang rust --out-dir zeughaus-sync/src/module_bindings --module-path zeughaus-module
```

Host and `spacetimedb-sdk` must share a major/minor version or the wire
format mismatches on connect. `join <host[:port]/database>` on either binary
joins another machine's session; the runner prints the token. The runner
keeps `runner.pem` and `client.pem` under the state directory
(`--state-dir`, `ZEUGHAUS_STATE_DIR`, else `~/.local/state/zeughaus`); a
remote editor needs `client.pem` copied into its own state directory. An
editor without a store edits a local scratch graph and shows no values.

In the editor: `Ctrl+Space` opens the command palette (spawn nodes, save/load
a `.zgh` file, auto layout, copy the session token, attach or close a job's
terminal, hold or release the runner). `+` opens a terminal tab, `H`/`V`
split a pane with a terminal, `x` closes one and kills its child (a job's
terminal is only detached); `Ctrl+Shift+T` takes control of a terminal
someone else drives, `Ctrl+Shift+C`/`V` copy and paste, `Ctrl+Shift+Escape`
gives the keyboard back to the app. A `Job` node runs its `command` line
when its `run` pin fires or it is pressed; a failed run keeps its terminal
for attaching, every run keeps `<state-dir>/runs/<id>/log`.
`zeughaus-runner trigger <endpoint> <node-id> [payload]` and
`zeughaus-runner hold <endpoint> on|off` do the same from a script, against
the endpoint URL a runner prints at start.

## Development Workflow

Plan, implement the minimal version, fix what you observe, refactor, commit,
push only after the gate:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo check --target wasm32-unknown-unknown -p zeughaus`

Iterate with `cargo check -q --message-format short -p <crate>`. One cargo
invocation per workspace at a time; long runs get a generous timeout.

## Conventions

- Commits: `type(scope): summary`, one line, max 60 characters, imperative,
  why over what. Types: feat, fix, docs, chore, refactor, test, style, perf.
- No emojis in code, comments, docs or console output. All file content in
  English. Comments state constraints, invariants and reasons; never the
  history of how the code got here.
- Rust navigation and refactoring go through the language server
  (rust-analyzer): definitions, references, rename, diagnostics. Text search
  is for string literals, comments and non-Rust files.
- A node is configured through settings only: text in, the node parses and
  refuses with `ZeughausError::InvalidParameter`. No process matches on a
  node's type id to interpret its parameters.
- The editor never executes and does not depend on `zeughaus-runtime`; what
  it shows is what the runner reported.
- Runner and editor register the same plugin list in the same order; a
  native-only plugin is gated in the editor, not omitted from the runner.
- Anything that travels between processes has one definition: store rows in
  `zeughaus-core::document`, runtime traffic in `zeughaus-link`, terminals
  in `zeughaus-mux`. Changing a wire type means changing both ends in one
  commit.
