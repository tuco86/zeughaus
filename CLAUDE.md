# Claude Code Instructions for Zeughaus

## Project Overview

Zeughaus is a visual dataflow workbench built on iced and iced_nodegraph.
It connects heterogeneous tools (DLL injection, databases, AI pipelines, process instrumentation)
through a unified node graph interface with pluggable domain subgraphs.

**Design Document**: See `DESIGN.md` for the full architecture specification.

**Status**: MVP functional. Editor opens, nodes can be created via command palette (Ctrl+Space),
connected, and data flows live through the graph.

## Workspace Structure

```
zeughaus/              # binary (zeughaus) - iced editor UI; edits the graph and views results, never executes
zeughaus-runner/       # binary (zeughaus-runner) - headless process that executes the graph and publishes results
zeughaus-sync/         # SpacetimeDB client shared by both binaries (generated bindings + connect/subscribe/publish)
zeughaus-core/         # types, traits (Ty, Typed, Value, Image, ExecutableNode, DomainPlugin)
zeughaus-runtime/      # graph execution engine (topo sort, dirty propagation, edge cache)
zeughaus-samples/      # the sample feed wire format (scaled video frames over QUIC), shared by runner and editor
zeughaus-transform/    # transform plugin (35 math/logic/string/trig nodes)
zeughaus-capture/      # capture plugin (Screen Capture -> frame/dimensions; xdg-desktop-portal on Wayland, scrap/X11/DXGI otherwise)
zeughaus-llm/          # LLM plugin (Conversation nodes, LM Studio chat)
zeughaus-ml/           # ML plugin (Keras layers as nodes -> exportable functional-API code)
zeughaus-flow/         # flow plugin (Hold: Event -> State; Button: manual Event; Timer: the clock a source node needs)
zeughaus-graph/        # graph plugin (Subgraph: a container node; Input/Output: the pins it shows its parent)
zeughaus-db/           # database plugin (Database container, Table, Insert, Query, SQL; tables designed in the graph -- field rows are bidirectional pins, a wire between two is a FOREIGN KEY; SQLite, native only)
zeughaus-record/       # record plugin (Recorder: frames + values to disk; Player: the same dataset as a source)
zeughaus-module/       # SpacetimeDB server module (excluded from the native workspace)
zeughaus-mux/          # the terminal mux wire model: stable ids, workspace topology, terminal rows/deltas, bounded codec, client-side TerminalView cache (pure, wasm too)
zeughaus-terminal/     # the runner's terminal engine: PTY sessions over portable-pty and a pinned wezterm-term, delivered as mux heads/deltas (native only)
iced_terminal/         # the terminal widget: one wgpu primitive per pane drawing a TerminalView, input to TerminalCommands, bundled ComicShannsMono Nerd Font
iced_tabs/             # the tab bar the workspace shell uses
```

## Related Projects

- `C:/workspace/iced_nodegraph` - The node graph widget library this project builds on
- `C:/workspace/tamagotchi` - DLL injection and memory analysis code (future plugin source)
- `C:/workspace/arma3-afsc-rs` - Screen capture + TensorFlow inference pipeline (reference for video/AI nodes)

## Development Workflow

**Phases:**
1. **Plan** - Design the implementation strategy thoroughly before writing code
2. **MVP** - Implement minimal working version of the feature
3. **Fix** - Address all observed errors and issues
4. **Refactor** - Improve code quality, structure, and readability
5. **Commit** - Once code is clean, create a commit
6. **Push** - Only after all checks pass

**Pre-Push Checklist:**
- `cargo check` - native compilation
- `cargo test` - unit tests
- `cargo clippy -- -D warnings` - lints

## Git Commit Message Rules

**Format**: `type(scope): summary` (Conventional Commits)

**Types**: `feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `style`, `perf`

**Rules**:
- Single line only, max 60 characters
- Imperative mood: "add", "fix", "remove"
- Focus on WHY, not WHAT

## Documentation Standards

- **NO EMOJIS** in code, comments, documentation, or console output
- Clear, technical, professional language
- All file content in English

## Tool Usage Preferences

**Rust code navigation: Always prefer rust-analyzer via cclsp MCP.**

When navigating, understanding, or refactoring Rust code, use the LSP tools as the
primary method. They provide type-aware, semantically correct results that Grep/Glob
cannot match (e.g. distinguishing a struct field from a local variable with the same name).

| Task | Tool |
|------|------|
| Find definition | `mcp__cclsp__find_definition` |
| Find all usages / references | `mcp__cclsp__find_references` |
| Find implementations of trait | `mcp__cclsp__find_implementation` |
| Rename symbol (safe refactor) | `mcp__cclsp__rename_symbol` |
| Get compiler diagnostics | `mcp__cclsp__get_diagnostics` |
| Hover for type info / docs | `mcp__cclsp__get_hover` |
| Call hierarchy (incoming) | `mcp__cclsp__get_incoming_calls` |
| Call hierarchy (outgoing) | `mcp__cclsp__get_outgoing_calls` |
| Search symbols by name | `mcp__cclsp__find_workspace_symbols` |

**Fall back to Grep/Glob only for**: string literals, comments, non-Rust files,
regex patterns, or when the LSP server is unavailable.

## Architecture Notes

### Core Concepts
- **Runtime Type System**: Pins declare a `Ty` built at runtime (scalars, `List`, `Option`, `Record`, `Opaque`), not a compile-time string. `Typed::ty()` is the single source of truth for both a pin's declaration and a value's tag, so they cannot disagree. Nodes may derive their pins from what is connected (`sync_pins`).
- **Editor and Runtime are Separate Processes**: `zeughaus` edits and views, `zeughaus-runner` executes. They meet in the store, so a local editor and a remote one are the same thing. Runners register in `runtime` and the lowest `seq` owns execution (a second runner is a hot standby). Results travel over weida, not the store: outputs and node errors on `/events`, a late-join snapshot on `/snapshot`, trigger presses the other way on `/triggers`, frames on `/samples`, terminals on `/mux`. Only `bool`/`int`/`float`/`str` are values on the wire (`zeughaus-core/src/wire.rs`).
- **Sample Feed**: Frames never touch the store. The runtime binds a QUIC listener (`weida`), announces it in the `runtime` row, and a viewer holds one standing exchange per (node, pin): it names the size it draws, the runtime scales to a tier ladder and streams frames until the viewer stops. Backpressure is QUIC's, so a slow viewer gets fewer frames -- always the current one, never a backlog.
- **Weida Redials, the Editor Resyncs**: The editor dials each runtime address once (`zeughaus/src/transport.rs`, `first_dial`) and weida keeps it alive under a `ReconnectPolicy` (250 ms doubling to 4 s, never giving up), re-sending the event filter on the redialled connection. What a redial does not restore, `zeughaus/src/feed.rs` and `zeughaus/src/mux.rs` do on `PeerEvent::Connected`: a fresh `/snapshot`, a reopened feed exchange, a fresh mux attach. A runner that restarted is a new peer to weida; the new address in the `runtime` row is what replaces the tasks.
- **Terminal Mux**: The runner owns every terminal (`zeughaus-runner/src/mux`): `WorkspaceActor`-style state under one mutex for tabs/splits/panes, a `zeughaus_terminal::Session` per terminal, a lease per terminal (first typist acquires, `TakeControl` revokes, ten seconds of grace across a redial). One `/mux` replier serves three exchange kinds told apart by their first frame: control (attach = hello + topology + every head; then commands, replies, snapshots), terminal (full duplex, input up, coalesced deltas down, computed per subscriber from its last sequence number -- every retained row written since, wherever it scrolled to), row fetch (short). The editor keeps a `TerminalView` per terminal (`zeughaus-mux/src/view.rs`): heads replace, deltas apply only at their exact base, pages fill holes, the store is bounded around the viewport. `iced_terminal::Terminal` draws it and emits `Action`s; the app routes them (`App::apply_terminal_action`). A runner restart is a new incarnation and an empty workspace; terminals never migrate. See `DESIGN.md`, "Terminals: a runner-owned multiplexer".
- **A Source Needs a Clock**: Nothing upstream wakes a screen capture, so `ExecutableNode::tick_interval` lets a node ask to be run periodically and the host schedules it. `flow.timer` is that clock; without one in the graph a capture node produces exactly one frame and stops.
- **Push/Pull Reactive Dataflow**: Every edge has a last-value cache. Push notifies downstream, pull triggers lazy computation.
- **Trigger vs Sample Pins**: Input pins are either trigger (causes execution) or sample (read passively).
- **Atomic Flush**: Multi-output nodes buffer with emit/flush to ensure synchronized delivery.
- **Capture**: Opt-in per-node persistence of results to database. No event sourcing.
- **Which Inputs Just Arrived**: Dirty propagation reruns every downstream node, so a node that must act only on its own trigger asks `InputSet::changed(pin)` -- a delivery since its last run, tracked per edge by cache generation -- and `GraphExecutor::refresh_pins` re-reads a node's pins after a setting changed them (a `db.table`'s column list).
- **Domain Subgraphs**: Each domain (DLL inject, DB, AI, etc.) is a subgraph type with its own semantics. A container node holds children by `parent` id and shows their `graph.input`/`graph.output` boundaries as its own pins; only the editor knows about the nesting (DESIGN.md).
- **Editor/Executor Separation**: Implemented as two processes (see above). Also what enables a WASM browser editor against native execution.

### Technology Stack
- UI: iced 0.14 + iced_nodegraph
- Collaboration: SpacetimeDB 2.10 (`spacetimedb-sdk`, `browser` feature on wasm32)
- Async: Tokio
- Local Storage: SQLite
- Production DB: PostgreSQL
- IPC: gRPC (tonic)
- Serialization: serde

### WASM
`cargo check --target wasm32-unknown-unknown -p zeughaus` is green. Native-only
plugins (capture) and `rfd` dialogs are behind `cfg(not(target_arch = "wasm32"))`;
`trunk serve` in `zeughaus/` serves the browser editor (WebGPU only, no WebGL
fallback).

### Running it
Three processes, in this order:

```
spacetime start                 # store, 127.0.0.1:3000, data in ~/.local/share/spacetime/data
cargo run -p zeughaus-runner    # executes the graph and owns the terminals; nothing runs without it
cargo run -p zeughaus           # editor; start as many as you like
```

The runner keeps its identity and one client identity under the state
directory (`$ZEUGHAUS_STATE_DIR`, else `$XDG_STATE_HOME/zeughaus`, else
`~/.local/state/zeughaus`; `--state-dir` on the runner): `runner.pem` is what
the announced `weida://sha256:<fp>@..` URL pins, so the fingerprint survives a
restart; `client.pem` is bootstrapped by the runner and presented by every
editor on the same machine. The listener requires a trusted client
certificate (that one, plus any `clients/*.pem`), and a bind on anything but
loopback without client trust refuses to start. A remote editor needs
`client.pem` copied into its own state directory. In the editor, `+` opens a
terminal tab, `H`/`V` split a pane with a terminal, `x` closes one (and kills
its child); `Ctrl+Shift+T` takes control of a terminal someone else drives,
`Ctrl+Shift+C`/`V` copy and paste, `Ctrl+Shift+Escape` gives the keyboard
back to the app.

An editor with no runner still edits the graph -- it just shows no values, and
says so in the status bar. Publish the module once per schema change:

```
spacetime publish --server local zeughaus --module-path zeughaus-module
spacetime generate --lang rust --out-dir zeughaus-sync/src/module_bindings --module-path zeughaus-module
```

`spacetime generate` output is checked in. The CLI renamed `--project-path` to
`--module-path` in 2.7. Host and `spacetimedb-sdk` must share a major/minor
version or the wire format mismatches on connect.
