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
iced_tabs/             # the tab tree the workspace shell uses: runner sections, groups, drop markers
zeughaus-theme/        # the editor's theme: iced theme paired with a terminal colour scheme, catalogs for every widget, bundled pack, WezTerm scheme parser
vm/win11/              # scripts: headless Windows 11 guest under QEMU/KVM, the CI runner's `win11` machine (boot, toolchain, per-boot prepare.ps1)
deploy/                # install.sh, systemd user units for store and runner, desktop entry template; install-ci.sh and ci/ for the CI runner
third_party/           # its own workspace: wezterm's terminal crates at the pinned revision, published as zeughaus-* packages
```

Sibling checkout this workspace depends on by path: `../weida` (the QUIC
transport), with the version of a published weida release next to the path.
The node graph widget is `iced_nodegraph` from crates.io.

Every crate of the workspace is on crates.io under the workspace version;
`main` carries the next version with a `-dev` suffix. `RELEASING.md` is the
release checklist, `CHANGELOG.md` the notes. `third_party/` keeps upstream
code apart from this gate; its header in `third_party/Cargo.toml` lists the
only changes made to it.

## Running it

The user's stack is installed, not run from the checkout: `deploy/install.sh`
`cargo install`s the editor and runner into `~/.cargo/bin`, installs two
systemd user units -- `zeughaus-store` (`spacetime start`, 0.0.0.0:3000)
and `zeughaus-runner` (requires the store, `KillMode=process` so the shims
and their shells outlive a stop, `reload` = SIGUSR1) -- plus a desktop entry
and icon matching the editor's Wayland app id, publishes the module, then
starts or reloads the runner and restarts running editors. The editor is
started from the application menu (`Zeughaus`). Logs: `journalctl --user -u
zeughaus-runner` / `-u zeughaus-store`.

By hand, three processes, in this order:

```
spacetime start                 # store, 0.0.0.0:3000
cargo run -p zeughaus-runner    # executes the graph and owns the terminals
cargo run -p zeughaus           # editor; start as many as you like
```

Never next to the installed units: a second store fails on the port, and a
second runner on the same state directory refuses to start (it holds
`<state-dir>/runner.lock`).

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

In the editor: `Ctrl+Space` (on macOS also `Cmd+Shift+P`) opens the command
palette (spawn nodes into the
focused graph pane, save/load a `.zgh` file, auto layout, pick a theme, copy
the session token, split or close the focused pane, attach or close a job's
terminal, hold or release a runner), and `Ctrl+PageDown`/`Ctrl+PageUp`
switch to the next or previous tab, also from a terminal, whose child no
longer receives them. The window draws its
own titlebar; the button in its corner moves the tab bar between the top and
the left edge. Theme and tab bar placement persist in
`<state-dir>/editor.toml`; WezTerm colour schemes dropped into
`<state-dir>/themes/` appear as themes. The tab bar has one section per
connected runner (plus `Local` without a store, or `Not running` for graphs
no runner's workspace shows); a section's `+ Shell`, `+ Graph` and `+ Group`
add a terminal tab, a graph that runner executes, or a
coloured group, and a CI runner's `CI: busy|free` cycles its busy mode
through auto (the GPU measurement), busy and free. Closing a graph's tab
deletes the graph. Double-clicking a
tab or a group's name renames it; a tab that shows one graph renames the
graph. A container node's `open` opens its contents in a tab. With the bar
on top or at the left, tabs are dragged into and
out of groups, onto another tab's content to split it, and a pane of a split
tab is dragged by its grip back into the bar; a tab dropped on a tab lands
before or after it by the half under the pointer, and the bar's empty rest
is the end of the last section. "Pane / Split Horizontal|Vertical" in the palette splits
the focused pane with a terminal, "Pane / Close" closes it and kills its
child (a job's terminal is only detached); `Ctrl+Shift+T` takes control of a
terminal someone else drives, `Ctrl+Shift+C`/`V` copy and paste,
`Ctrl+Shift+Escape` gives the keyboard back to the app (on macOS also
`Cmd+T`/`C`/`V`/`Escape`; left Option is Meta, right Option composes), and
`Ctrl`+click opens a terminal's hyperlink. A
`Job` node runs its
`command` line when its `run` pin fires or it is pressed; a failed run keeps
its terminal for attaching, every run keeps `<state-dir>/runs/<id>/log`.
`zeughaus-runner trigger <endpoint> <node-id> [payload]` and
`zeughaus-runner hold <endpoint> on|off` do the same from a script, against
the endpoint URL a runner prints at start; a triggered run's terminal lands
in that runner's locked `Triggered` group.

Every terminal lives in a shim process (`zeughaus-runner shim <dir>`, one
per terminal under `<state-dir>/terminals/`), so shells and job runs
survive the runner. `SIGUSR1` makes the runner save
`<state-dir>/workspace.json` and `exec` its binary again; the new process
reattaches every shim and restores tabs and splits. The editor answers
`SIGUSR1` the same way: it saves its window size, tabs, cameras, selection,
node sizes, palette, rename and terminal scroll-back to a restore file and
reopens as it was (not where it was: Wayland does not let a client place
its window).

### CI runner

A second runner runs CI as the Unix user `zeughaus-ci`
(`deploy/install-ci.sh`, once, with sudo; `deploy/install.sh` updates it
afterwards). Its home and state are `/var/lib/zeughaus-ci` (a btrfs
subvolume outside snapper's snapshots). It has two user units,
`zeughaus-ci-runner` (joins the store as a runner) and `zeughaus-ci-hook`
(`ci hook --listen 10.8.0.10:8686`, the webhook intake that Caddy on sadala
forwards `https://ci.doodleshnookie.net/hook/<repo>` to). Logs:
`sudo journalctl _UID=$(id -u zeughaus-ci)`. Repositories, budgets and the
VM are configured in `/var/lib/zeughaus-ci/state/ci.toml`, and secrets are
files in `state/secrets/`. `zeughaus-ci run|status|forge-check` (a sudo
wrapper) queues a pipeline and reads results. `zeughaus-runner ci check
[DIR]` and `ci plan [DIR] <push|tag> <ref>` validate a `.ci/` folder without
a runner. The Windows VM (`vm/win11/`, installed under
`/usr/local/lib/zeughaus-ci/vm/win11`) is booted and stopped by the
runner. `DESIGN.md` section 14 describes how it works.

This repository is one of its repos: every push to Forgejo runs
`.ci/gate.sh` (the gate below, in the `.ci/arch.Containerfile` image,
against weida's `main` as the sibling checkout) and posts `zeughaus/gate`
on the commit. Forgejo reaches the runner as the bot user `zeughaus-ci`
(write on zeughaus, read on weida); its tokens and the webhook secret are
in OpenBao under `secret/zeughaus/ci`. A change to `.ci/` is checked with
`zeughaus-runner ci check` before it is pushed.

### Agent stack and reload

An agent never drives the user's desktop. It builds its own binaries with
the `remote` feature into a separate target directory (so it neither
replaces the user's binaries nor holds their build lock), runs them against
its own database and state directory, and drives a headless editor over a
control socket:

```
spacetime publish --server local zeughaus-agent --module-path zeughaus-module -y
CARGO_TARGET_DIR=target/agent cargo build -p zeughaus --features remote -p zeughaus-runner
ZEUGHAUS_STATE_DIR=/tmp/zh-agent target/agent/debug/zeughaus-runner join 127.0.0.1:3000/zeughaus-agent
ZEUGHAUS_STATE_DIR=/tmp/zh-agent target/agent/debug/zeughaus --headless --control /tmp/zh-agent.sock join 127.0.0.1:3000/zeughaus-agent
target/agent/debug/zeughaus ctl /tmp/zh-agent.sock key ctrl+space
target/agent/debug/zeughaus ctl /tmp/zh-agent.sock screenshot /tmp/shot.png
```

The control commands are `size`, `move X Y [MS]`, `down`, `up`,
`click X Y [BUTTON] [MODS]`, `dblclick`, `drag X1 Y1 X2 Y2 [STEPS] [MS]`,
`scroll X Y DY [DX]`, `key`,
`type`, `find`, `screenshot`, `record PATH`, `record-stop`, `resize`,
`scale`, `clip`, `clip-set`, `wait-idle`, `restart`, `quit` (coordinates in
logical pixels; `MS` paces a move or drag). `ctl` resolves a relative
`screenshot`/`record` path against its own working directory. `record`
writes an MP4 through `ffmpeg` with the pointer drawn in, for attaching to
an issue or PR by hand; screenshots and videos are never committed.
Signals to the agent's processes go by PID, never by name.

When a change is finished, its gate passed and it is committed, the agent
integrates it into the user's running stack; that is the point of the
reload. `deploy/install.sh` does it in order: `cargo install` of both
binaries (the running ones keep serving meanwhile), units, desktop entry,
store start, `spacetime publish --server local zeughaus` without `-c`/`-y`
(a migration that needs the data cleared stops the script and is the
user's call, never the agent's), `systemctl --user reload zeughaus-runner`,
and SIGUSR1 to every editor whose `/proc/<pid>/exe` is the installed
binary and whose `SigCgt` has the handler. Confirm the reload: `readlink
/proc/<pid>/exe` no longer ends in `(deleted)`. The agent's own processes
run from `target/agent` and are never signalled.

Before signalling a process, check it has a restart handler: bit 9
(`0x200`, SIGUSR1) in `SigCgt` of `/proc/<pid>/status`. A process built
before the reload existed dies from SIGUSR1 instead, and a runner that old
also keeps its shells in-process -- the agent's own session may be one of
them (`ps --ppid <runner-pid>`). That one switch is the user's to make, by
restarting the process by hand.

## Development Workflow

Plan, implement the minimal version, fix what you observe, refactor, commit,
push only after the gate:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo check --target wasm32-unknown-unknown -p zeughaus`

A change to a crate's manifest or to what it includes also passes
`cargo publish --workspace --dry-run`.

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
