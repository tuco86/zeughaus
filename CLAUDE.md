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
zeughaus/              # binary (zeughaus.exe) - iced UI + iced_nodegraph
zeughaus-core/         # types, traits (Value, ExecutableNode, DomainPlugin)
zeughaus-runtime/      # graph execution engine (topo sort, dirty propagation, edge cache)
zeughaus-transform/    # transform plugin (37 math/logic/string/trig nodes)
zeughaus-process/      # process plugin (Find Process, DLL Inject, Read Memory, Find Module)
zeughaus-capture/      # capture plugin (Screen Capture via scrap/DXGI)
zeughaus-llm/          # LLM plugin (Conversation nodes, LM Studio chat)
zeughaus-ml/           # ML plugin (Keras layers as nodes -> exportable keras.Sequential code)
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
- **Push/Pull Reactive Dataflow**: Every edge has a last-value cache. Push notifies downstream, pull triggers lazy computation.
- **Trigger vs Sample Pins**: Input pins are either trigger (causes execution) or sample (read passively).
- **Atomic Flush**: Multi-output nodes buffer with emit/flush to ensure synchronized delivery.
- **Capture**: Opt-in per-node persistence of results to database. No event sourcing.
- **Domain Subgraphs**: Each domain (DLL inject, DB, AI, etc.) is a subgraph type with its own semantics.
- **Editor/Executor Separation**: Editor designs graphs, runners execute them. Enables WASM browser editor with native execution.

### Technology Stack
- UI: iced 0.14 + iced_nodegraph
- Collaboration: SpacetimeDB (WASM support incoming via PR #4183)
- Async: Tokio
- Local Storage: SQLite
- Production DB: PostgreSQL
- IPC: gRPC (tonic)
- Serialization: serde

### WASM
Deferred until SpacetimeDB WASM support lands. No wasm32 target for now.
