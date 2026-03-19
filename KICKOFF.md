# Kickoff Prompt

Copy-paste this as your first message in a new Claude Code session started in `C:/workspace/zeughaus/`:

---

Read DESIGN.md and CLAUDE.md thoroughly. This is a new project -- no code exists yet.

Before writing any code:

1. **Setup Phase**
   - Initialize a Cargo workspace
   - Set up crate structure based on the architecture in DESIGN.md (editor, runtime, plugin trait, storage)
   - Configure for both native and `wasm32-unknown-unknown` targets
   - Ensure rust-analyzer works (open a dummy lib.rs, check diagnostics via LSP)
   - Add iced 0.14, tokio, serde as initial dependencies where appropriate
   - git init, initial commit

2. **Planning Phase**
   - Enter plan mode
   - Read the DESIGN.md carefully and break the implementation into phases:
     - Phase 1: Core types (Event, EventMeta, PinKind, DataMode, EdgeState, NodeConfig)
     - Phase 2: Plugin trait (DomainPlugin, ExecutableNode, PinDefinition)
     - Phase 3: Runtime (topological sort, dirty-flag propagation, push/pull execution)
     - Phase 4: Editor integration (iced_nodegraph as the UI, node catalog, command palette)
     - Phase 5: First domain plugin (a simple Transform domain as proof of concept)
   - For each phase, identify the exact files to create, types to define, and tests to write
   - Present the plan for review before proceeding

3. **Implementation Phase**
   - Work through the plan phase by phase
   - After each phase: run cargo check, cargo test, cargo clippy
   - Commit after each passing phase
   - Do not ask for input unless you hit a genuine design ambiguity -- refer to DESIGN.md for decisions

Work autonomously. Minimize interruptions. Use plan mode for planning, then exit and execute.
