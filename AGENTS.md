# Agent instructions for Zeughaus

## Project Overview

Zeughaus is a visual dataflow workbench built on iced and iced_nodegraph. A
graph of nodes -- transforms, screen capture, SQLite schemas, LLM
conversations, Keras models, recordings -- is edited collaboratively and
executed by a headless runner that also multiplexes terminals for its editors.

**Architecture**: `DESIGN.md` is the one architecture document. Read it
before changing how the processes talk to each other, how values travel, or
how a node is configured. Everything above its "Not built" section describes
the code as it is.

**Status**: working. Editor and runner talk over weida; each runner holds and
persists its own graphs, and editors edit them through it. Collaborative
editing, live values, video feeds, subgraphs, drawn database
schemas, Keras export, recordings and terminal panes are in use. The browser
build of the editor compiles and edits locally (it reaches no runner yet).

## Workspace Structure

```
zeughaus/              # binary: the iced editor; edits the graph and views results, never executes
zeughaus-runner/       # binary: headless process that executes the graph, owns the terminals, runs the CI jobs it is assigned and follows a release channel; `install` writes a host's profile, `update status|rollback` and `version` report and undo updates, `release` makes and checks signed bundles; `mcp` gives an agent the CI and manual job runs as tools
zeughaus-ci/           # binary: the CI service; schedules pipelines onto enlisted runners, webhook intake, manual-run uploads, stored transcripts; follows a release channel itself (`install ci`, `update`, `version`)
zeughaus-update/       # the self-updater both binaries share: signed manifest, fetch, preflight, install with `.previous`, trial and rollback, and the helpers of the `install` profiles (`host`)
zeughaus-core/         # Ty/Typed/Value, pins, ExecutableNode/DomainPlugin, settings, scalar wire encoding, graph document types
zeughaus-runtime/      # GraphExecutor: topology, node instances, edge cache, dirty set, async work, node errors
zeughaus-link/         # the runner<->editor protocol over weida (graph, feed, events, snapshot, triggers, paths), the runner<->CI enlist protocol, the `/manual` upload of working-tree snapshots and the credentials both ends present
zeughaus-mux/          # terminal mux wire model: stable ids, workspace topology, rows/deltas, bounded codec, client-side TerminalView (pure, wasm too)
zeughaus-terminal/     # the terminal engine: PTYs over portable-pty and a pinned wezterm-term in the runner, byte-fed sessions for the CI's transcripts (native only)
zeughaus-transform/    # plugin: 35 math/logic/string/trig nodes, constants, Display
zeughaus-flow/         # plugin: Hold (event -> state), Button (manual event), Timer (the clock a source needs), All (fan-in)
zeughaus-graph/        # plugin: Subgraph container and its Input/Output boundary nodes
zeughaus-ml/           # plugin: Keras layers as nodes -> exportable functional-API code
zeughaus-llm/          # plugin: Conversation nodes against an LM Studio endpoint
zeughaus-capture/      # plugin: Screen Capture (xdg-desktop-portal on Wayland, scrap otherwise)
zeughaus-db/           # plugin: Database container, Table, Insert, Query, SQL; schemas drawn in the graph (SQLite)
zeughaus-record/       # plugin: Recorder (frames + values to disk) and Player (the same dataset as a source)
zeughaus-job/          # plugin: Job (a process in a runner-owned terminal, log per run under the state dir) and the ProcessHost trait the runner implements
iced_terminal/         # the terminal widget: one wgpu primitive per pane, bundled ComicShannsMono Nerd Font Mono (the editor bundles the Propo variant as its UI and icon face)
iced_tabs/             # the tab tree the workspace shell uses: runner sections, groups, drop markers
zeughaus-theme/        # the editor's theme: iced theme paired with a terminal colour scheme, catalogs for every widget, bundled pack, WezTerm scheme parser
vm/win11/              # scripts: headless Windows 11 guest under QEMU/KVM, the service runner's `win11` machine (boot, toolchain, per-boot prepare.ps1); toolchain.ps1 also provisions a Windows runner host
deploy/                # bootstrap.sh (downloads, checks and runs a build's `install <profile>`), setup-service-runner.sh, setup-sadala.sh, setup-windows.sh (the one-time part of a host); systemd user unit for the runner, desktop entry template, macos-app.sh and the bundle's Info.plist; runner/ (unit, ssh config, enlist.toml.example) and ci/ (CI unit, ci.toml.example, openbao.sh), macos/ (launchd agent), windows/ (host setup, bootstrap.ps1, maintain.toml)
third_party/           # its own workspace: wezterm's terminal crates at the pinned revision, published as zeughaus-* packages
omp-plugin/            # the zeughaus-ci omp plugin (skill + extension); catalog in .omp-plugin/marketplace.json; bump its three versions together
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

The user's stack is installed, not run from the checkout, and updates itself:
`sh deploy/bootstrap.sh desktop` downloads the newest `dev` build, checks the
archive against its `SHA256SUMS` line, unpacks it and runs its
`zeughaus-runner install desktop`. That puts the editor and runner binaries
into `~/.cargo/bin`, the systemd user unit `zeughaus-runner`
(`KillMode=process` so the shims and their shells outlive a stop, `reload` =
SIGUSR1), a desktop entry and icon matching the editor's Wayland app id
(icon cache and `kbuildsycoca6` refreshed) in place, writes
`<state-dir>/update.toml` (`channel = "dev"`) and `install.toml`, then
enables and starts or reloads the runner and restarts running editors. After
that the runner follows `dev` by itself (see Updates below); the checkout is
needed to run the bootstrap and to develop, not to deploy. The editor is
started from the application menu (`Zeughaus`). Logs: `journalctl --user -u
zeughaus-runner`.

By hand, two processes, in this order:

```
cargo run -p zeughaus-runner    # holds and executes the graphs, owns the terminals
cargo run -p zeughaus           # editor; start as many as you like
```

Never next to the installed unit: a second runner on the same state
directory refuses to start (it holds `<state-dir>/runner.lock`).

The runner keeps `runner.pem` and `client.pem` under the state directory
(`--state-dir`, `ZEUGHAUS_STATE_DIR`, else `~/.local/state/zeughaus`), its
graphs in `graphs/<id>.zgh` (one file per top-level graph) and its pinned URL
in `endpoint`. An editor dials the runner of its own state directory through
that file and every URL in `remotes = [..]` of `<state-dir>/zeughaus.toml`
(CIs are listed next to them as `ci = [..]`);
a remote editor also needs `client.pem` copied into the remote runner's
`clients/` directory and its own state directory. An editor without a
runner edits a local scratch graph and shows no values.

In the editor: `Ctrl+Space` (on macOS also `Cmd+Shift+P`) opens the command
palette (spawn nodes into the
focused graph pane, save/load one graph as a `.zgh` file, auto layout, pick a theme,
split or close the focused pane, attach or close a job's
terminal, hold or release a runner), and `Ctrl+PageDown`/`Ctrl+PageUp`
switch to the next or previous tab, also from a terminal, whose child no
longer receives them. The window draws its
own titlebar; the button in its corner moves the tab bar between the top and
the left edge. Theme and tab bar placement persist in
`<state-dir>/editor.toml`; WezTerm colour schemes dropped into
`<state-dir>/themes/` appear as themes. The tab bar has one section per
connected runner (plus `Local` without a runner, or `Not running` for graphs
no runner's workspace shows); a section's terminal, graph and folder
glyphs (tooltips say which) add a terminal tab, a graph that runner
executes, or a coloured group. A `CI <label>` section for each CI the editor
dials (the `ci = [..]` URLs) holds a `Runners` tab with the enlisted runners
and their machines and a locked group per repository of `ci.toml` with
its Plans tab (below) and one tab per channel (pipelines one row each, newest on
top and open with its jobs, a cable animating while its artifact set moves,
failures with their cause and log excerpt,
`Rerun` and `Cancel`; pressing a job shows its transcript read-only, live
while it runs; Up/Down and Enter walk and open the list, Escape closes the
transcript). The play/pause circle (filled when set by
hand) sits on the section of the runner that has `[busy]` (`tuco-pc`) and
cycles the busy mode through auto (the GPU measurement), busy and
free. Channels come from `.zeughaus-ci/channels.toml`. Closing a graph's tab
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
`command` line when its `run` pin fires or it is pressed; its terminal lives
as long as the program (the run ends with the program's exit code), every run
keeps a timestamped WAL in `<state-dir>/runs/<id>/wal`, and every shell one
in its shim directory.
`zeughaus-runner trigger <endpoint> <node-id> [payload]` and
`zeughaus-runner hold <endpoint> on|off` do the same from a script, against
the endpoint URL a runner prints at start; a triggered run's terminal lands
in that runner's locked `Triggered` group, which exists only while it holds
a tab.

`zeughaus-runner [--state-dir <dir>] mcp [--ci <url>]` is an MCP server on
stdio, for an agent that starts it as a tool server. It dials the CI that
`--ci` names, else the first `ci = [..]` of `<state-dir>/zeughaus.toml`, with
that directory's `client.pem`, so it is the same principal as an editor
there; without either it still starts and every tool answers with what is
missing. `ci_status`, `ci_pipelines`, `ci_pipeline`, `ci_log`, `ci_rerun`
and `ci_cancel` read and control the CI's channels, pipelines and job
output (`ci_log` shows the last lines or the line ranges asked for, each
with its number and, on request, the seconds since the job's first
output); `ci_wait` waits until a pipeline (by number, or by the sha of
the commit just pushed) or one job of it has settled; `ci_timing` says where
a pipeline's time went (for each job when it was assigned, started and ended
after the pipeline was created, its staging time and the inputs that moved
to it) or, for one job, which lines of its output the longest pauses follow,
from the time the terminal WAL keeps for every chunk. `run_jobs` snapshots
the checkout of the current directory (its commit, the changes to it and
the untracked files that are not ignored) and the sibling checkouts its
Cargo path dependencies point to, uploads them to the CI as git bundles
with the names of jobs of the repository's pipeline, and answers as
`ci_wait` does once that manual pipeline has settled, or with how to
continue with `ci_wait`. `ci_repo` says which repository of the CI the
checkout is (by its origin), whether its default branch and `HEAD` are
green, and every job of its `.zeughaus-ci/` with the runner and place it
runs at; in such a checkout `repo` can be omitted from every other tool,
and `ci_wait` without a number or sha waits for `HEAD`. `docs`, `doc_read`,
`proposal_write` and `plan_done` read and write the repository's plans and
proposals (below). Nothing but
protocol messages is written to its stdout.
`zeughaus-runner [--state-dir <dir>] ci repo [--json] [<path>]`, `ci
find [--json] <repo> <sha>`, `ci doc [--json] <repo> <n>`, `ci plan-save
[--json] [--approve] <repo> <file>` and `ci planned <repo> <file>` answer
the same for a script (exit 3: the origin is no repository of `ci.toml`;
2: no CI, no answer or a refusal); the
`zeughaus-ci` omp plugin in `omp-plugin/` uses them for its session
context, its push watch and `/zeughaus save`.

Plans and proposals: the CI keeps them per repository in
`<state-dir>/ci/docs/<repo>/<n>.json` (numbers shared by both kinds, never
reused). A proposal is `open`, then `planned` (a plan names it in a
`Proposals: #3, #5` line under its title) or `dismissed`; a plan is
`saved`, `approved` in the editor, `in_progress` once a master claims it
(only one per repository), then `done` (`plan_done`) or `rejected`. Each
change is announced as `CiDocs` on `/events`. A CI section's repository
group starts with its Plans tab, whose `Plan session` and `Master` buttons
ask the editor's own runner on `/agent` (Unix) to open `zeughaus-runner
plan <repo>` or `master <repo>` in a terminal of its `Triggered` group, in
the checkout `~/work/<origin>` (`$ZEUGHAUS_WORK` overrides `~/work`):
`plan` brings the detached worktree `<checkout>-plan` to `origin/<default
branch>` and execs omp in plan mode there; `master` claims approved plans
one after another and runs `omp @<plan> <instruction>` with
`ZEUGHAUS_PLAN=<repo>#<n>` in the main checkout, waiting while the
checkout is dirty or a plan is in progress. An open proposal's `Plan`
button opens `plan <repo> --proposal <n>` (tab `plan <repo> #<n>`), which
attaches the proposal to omp's first prompt so planning starts at once.
In every session `plan` starts (`ZEUGHAUS_PLAN_SESSION=<repo>`), the omp
plugin answers the agent's proposal with its own menu instead of omp's
review: `Save and approve` and `Save` store the plan (`ci plan-save
[--approve]`) and end the session, `Refine` keeps planning.

A runner also runs the tasks of `<state-dir>/maintain.toml` (`[[task]]` with
`name`, `command` as an argv, `every_minutes` and `timeout_minutes`,
default 30) by itself, one at a time and only while it is idle: each is a
run in the locked `Triggered` group, and while it runs the runner counts as
held, so a toolchain update never swaps files under a build. The file is read
every minute, a symlink is fine, and without it nothing runs. A task that
does not end with code 0 raises a desktop notification in every connected
editor; its log is `<state-dir>/runs/<id>/wal`.
`zeughaus-runner [--state-dir D] maintain` prints each task with its last
result and age from `<state-dir>/maintain/<name>.json`. The wohnzimmer
runner's file is `deploy/windows/maintain.toml`, which the release's `share/`
carries and `install windows` and every update write to its state directory.

Every terminal lives in a shim process (`zeughaus-runner shim <dir>`, one
per terminal under `<state-dir>/terminals/`), so shells and job runs
survive the runner. Under a systemd user manager each shim runs in a
transient scope of its own (`systemd-run --user --scope`): systemd-oomd
kills whole cgroups, and then takes one terminal instead of the runner and
every shell. `SIGUSR1` makes the runner save
`<state-dir>/workspace.json` and `exec` its binary again; the new process
reattaches every shim and restores tabs and splits. The editor answers
`SIGUSR1` the same way: it saves its window size, tabs, cameras, selection,
node sizes, palette, rename and terminal scroll-back to a restore file and
reopens as it was (not where it was: Wayland does not let a client place
its window).

### CI

CI is its own process, `zeughaus-ci`, that runners enlist into; it executes
nothing itself. On sadala (aarch64 Ubuntu) it runs as the Unix user
`zeughaus-ci` (home and state `/var/lib/zeughaus-ci`) in the user unit
`zeughaus-ci` (`deploy/ci/zeughaus-ci.service`: `serve --listen 10.8.0.1:7444
--hook 127.0.0.1:8686`; Caddy there forwards
`https://ci.doodleshnookie.net/hook/<repo>` to the hook). Logs:
`sudo journalctl _UID=$(id -u zeughaus-ci)` on sadala. Repositories, the
runners it accepts and the forge tokens are in
`/var/lib/zeughaus-ci/state/ci.toml` (`deploy/ci/ci.toml.example`);
secrets live in OpenBao (`secret/zeughaus/ci`, read with the AppRole that
`deploy/ci/openbao.sh`, run on sadala, seals to the CI user). Its binary is
`/var/lib/zeughaus-ci/bin/zeughaus-ci`, owned by that user, and it updates
itself (see Updates). The wrapper
`/usr/local/bin/zeughaus-ci` on sadala runs the binary as that user
(`sudo -u zeughaus-ci` with `--state-dir /var/lib/zeughaus-ci/state`):
`zeughaus-ci run|status|log|forge-check|secrets-check` queues a pipeline,
reads results, a failed job's excerpt and its log, and checks the forge
token and every secret `ci.toml` names. `zeughaus-ci check [DIR]` and `plan
[DIR] <push|tag> <ref>` validate a `.zeughaus-ci/` folder without a service;
`check` also places every job when the state directory has a `ci.toml`.

Five runners enlist, each with an `<state-dir>/enlist.toml`
(`deploy/runner/enlist.toml.example`: `ci` (the CI's pinned URL, the line in
its `endpoint` file), `name`, `cpus`, `budget_gb`, and the optional tables
`[containers]` (podman; Apple `container` on macOS, see below), `[busy]`
(nvidia-smi) and `[machines.<name>]` (a Windows
VM)). The CI pins each one by the fingerprint in its own `endpoint` file:

| host | runner | enlisted as | installed by |
|---|---|---|---|
| sadala | Unix user `zeughaus`, `10.8.0.1:7443`, binary in `/var/lib/zeughaus/bin` | `sadala` | `deploy/setup-sadala.sh` (also the CI) |
| tuco-pc | user `hannes`, `10.8.0.10:7443` (a local drop-in of its unit sets `--feed-addr`) | `tuco-pc-hannes` | `sh deploy/bootstrap.sh desktop` |
| tuco-pc | user `zeughaus` (home `/var/lib/zeughaus`, binary in `/var/lib/zeughaus/bin`), `10.8.0.10:7444`; containers, the `win11` VM, GPU busy | `tuco-pc`, the default runner | `deploy/setup-service-runner.sh --feed-addr 10.8.0.10:7444` |
| atik (macOS) | launchd agent `net.doodleshnookie.zeughaus-runner`, `10.8.0.7:7443`, log `~/Library/Logs/zeughaus-runner.log` | `atik` | `sh deploy/bootstrap.sh macos` |
| wohnzimmer (Windows 10, LAN `10.0.0.13`) | user `zeughaus`, boot task `zeughaus-runner` (S4U), `10.8.0.12:7443`, state and log `C:\zeughaus\state` (`runner.log`) | `wohnzimmer`, `os = "windows"`, builds `windows-amd64` | `deploy/setup-windows.sh [--setup] zeughaus@10.8.0.12` |

In `ci.toml`, `default_runner` names the runner of every job without a
`machine` or `platform`; `[runners.<name>]` has its `fingerprint`, its
`os` (`unix` by default, or `windows`), its
`machines` (VMs it boots) and `wait_minutes` (15: a job whose runner is
offline is skipped after that, not failed). `[platforms]` maps a platform
name (`linux-amd64`, `darwin-arm64`, ...) to the runner or VM that builds
it. A job header places a job: `machine = "<runner>"` runs on that runner's
host (`.sh`, `.ps1` on a Windows runner), `machine = "<vm>"` on the VM of
the runner that lists it
(`.ps1`), `platform = "<p>"` the same through `[platforms]` (exclusive with
`machine`), `image = "<image>"` in a container of the default runner, and
neither on the default runner's host. `image` together with `platform` runs
the container on the platform's runner, not on the default runner; a
platform that names a VM cannot have an image. `when_busy = "freeze"` needs
a container, a machine, or a platform that resolves to one: a host job
cannot be frozen. The job sees its platform as `CI_PLATFORM`. A runner that
is not in `ci.toml`, or whose host is not the configured `os`, is refused.
The Windows VM scripts (`vm/win11/`) are
installed under `/var/lib/zeughaus/lib/vm/win11` (written by `install
service` and again by every update) and booted and stopped by the `tuco-pc`
runner.

A repository's jobs at one place share one warm workspace on their runner:
`ci/work/<repo>/host` on the runner's host, `ci/work/<repo>/image-<image>`
in a container of that image, `ci/work/<repo>/machine-<vm>` on a VM.
Pipeline jobs and manual runs build in it one at a time, the oldest first
(by pipeline number, then the job's place in the pipeline); order between
jobs is their `needs`. A checkout fetches, checks out with `--force` and
runs `git clean -ffd`, so ignored files such as `target/` stay. `clean =
true` on a `[[channel]]` of `.zeughaus-ci/channels.toml` makes the first job
of that channel's pipelines at each place start from nothing (`git clean
-ffdx`). The header keys `context` and `cache` still parse and do nothing;
`zeughaus-ci check` warns about them.

A manual run executes named jobs of a repository's pipeline from the
uploader's working tree, uncommitted files and sibling path dependencies
(`../weida`) included. The uploader sends the CI's `/manual` path (editors'
keys only, wire types in `zeughaus_link::manual`) a `RunUpload` with the job
names and one git bundle per checkout; the upload's `origin` must be the
`url` of a repository of `ci.toml`. The CI reads `.zeughaus-ci/` of the
snapshot and runs the named jobs as a pipeline in the channel a push of the
default branch selects: a need on a job that is not named is dropped, no
job gets secrets, and a job on a Windows VM ends the pipeline as `error`.
Each job runs in the workspace of its place, where the pipeline builds too:
the main checkout is the workspace itself, the siblings are under
`<workspace>/.zci-siblings/<dir>` (`CI_SIBLINGS`), and the bundles stay in
`<state-dir>/ci/manual/<id>/` on the CI until the pipeline ended and reach
the runner on `/artifacts` (`ManualBundle`). A manual run posts no forge
status, raises no red alert and cannot be rerun; `/ci` answers a job's last
lines as text (`Log`) and lists the `[platforms]`.

The Windows runner `wohnzimmer` is the living-room PC, used by someone else
at its console. Everything of zeughaus is under `C:\zeughaus`, readable by
its account and SYSTEM only: `bin\` (binaries and the `runner.cmd` loop the
boot task runs), `state\`, `src\` (the `bootstrap.ps1` that
`setup-windows.sh` uploads), `cargo\`
and `rustup\`. Its WireGuard tunnel `sadala` (`C:\zeughaus\wg\sadala.conf`,
the WireGuard for Windows service) gives it `10.8.0.12`; the firewall admits
the runner from `10.8.0.0/24` only. It runs `.ps1` jobs on its host in
`C:\zeughaus\state\ci\work\<repo>\host`, has no containers, machines or
shims, and updates itself like the other hosts, but only while idle (not
held, no live run): the restart ends every terminal and the job it runs, as
the runner exits and the boot task's `runner.cmd` loop starts the new binary
about 5 s later. A running editor there keeps its build until it is
reopened.

On macOS the runner's container engine is Apple's `container` (Homebrew
formula `container`), not podman. The runner starts its system on demand
(`container system start --enable-kernel-install`) and never stops it; each
job container is a VM of its own that goes away with the job. Its clock can
run behind the host's while files written through the mounts carry the
host's time, so the container gets `CAP_SYS_TIME` and starts the job through
`clock.sh`: it sets the clock to the host's stamp on a file it creates in
the run directory plus 5 ms, and only warns when that fails. The job's
`inputs` are mounted writable, because `container` has no read-only mount,
and a job in such a container cannot be frozen: the runner logs that it
cannot and the job runs on. The runner offers containers only where
`[containers]` is in its `enlist.toml` and `container --version` answers.

Updates: no script builds from a checkout or copies binaries. A host is
bootstrapped once and then follows a release channel itself.
`deploy/bootstrap.sh <profile> [--channel C] [--feed-addr A] [--url U]`
(`desktop`, `macos`, `service` or `ci`; channel `dev` and url
`https://zeughaus.doodleshnookie.net/updates` by default) downloads the
channel's newest build, checks the archive against its `SHA256SUMS` line,
unpacks it and runs its `install`. It is the only download that is not
signature-checked (it trusts HTTPS to the update host); every later update
is. `deploy/setup-service-runner.sh --feed-addr <a>` (tuco-pc),
`deploy/setup-sadala.sh [--ci-toml <file>]` (from a workstation, last) and
`deploy/setup-windows.sh [--setup] [--feed-addr A] <ssh-host>` do a host's
root-only or remote one-time part (packages, users, trust, units, the old
binary's migration) and then run the bootstrap on it;
`deploy/windows/bootstrap.ps1 [-Channel dev] [-Url ...]` is the Windows
bootstrap. Rollout order: tuco-pc user, atik, the tuco-pc service runner,
wohnzimmer, and sadala last, so the CI keeps publishing throughout.

`zeughaus-runner [--state-dir D] install <desktop|macos|service|windows>
[--channel C] [--feed-addr A] [--bin-dir B] [--from DIR] [--no-service]`
writes a profile from an unpacked release (`--from`, by default the
directory of the running binary): `desktop` is the editor and runner into
`--bin-dir` (`$CARGO_HOME/bin` or `~/.cargo/bin`), the user unit, desktop
entry and icon, `systemctl --user` enable and reload or start, SIGUSR1 to
editors of that binary; `macos` the runner into `--bin-dir` and
`Zeughaus.app` into `~/Applications`, the launchd agent plist (`--feed-addr`
defaults to `10.8.0.7:7443`), `launchctl kill SIGUSR1` or `bootstrap`;
`service` (run as the service user; `--feed-addr` required) the runner into
`~/bin`, the VM scripts into `~/lib/vm/win11/`, `deploy/runner/ssh_config`
into `~/.ssh/config` and the unit with the feed address; `windows` both
executables into `--bin-dir` (required, `C:\zeughaus\bin`) and
`maintain.toml`, with no service control (the bootstrap script stops and
starts the task). `desktop` and `windows` refuse `--feed-addr`. Every
profile also writes `<state-dir>/update.toml` (`channel`) and
`<state-dir>/install.toml` (`profile`, `bin_dir`, optional `feed_addr`) and
prints `[install] <profile> from build <n>`. The non-binary files come from
the archive's `share/`, which holds repository files at their repository
paths; they are not compiled in. `zeughaus-ci [--state-dir D] install ci
[--channel C] [--from DIR] [--no-service]` does the same for the CI:
`zeughaus-ci` into `~/bin` (`/var/lib/zeughaus-ci/bin`), its unit, restart
or start. `zeughaus-runner version` and `zeughaus-ci version` print
`<version> build <n>`.

A runner or CI follows a release channel by itself when
`<state-dir>/update.toml` exists, and every host does: `channel = "dev"`
(`dev`, `nightly` or `release`) and optionally `url`. The CI hosts too, so
the safety net is the checks below, not a host left out. Once a minute it
reads `<url>/<channel>/latest`, a CI build number. A build newer than its own
(`ZEUGHAUS_BUILD` at compile time, the CI pipeline number; 0 for a build
made anywhere else, so a `cargo install` is replaced by the first published
one) is downloaded with `curl` into `<state-dir>/updates/<build>/`, its
`manifest.json` checked against `manifest.sig` with the keys compiled in
(`TRUSTED_KEYS`, `zeughaus-update/src/manifest.rs`), then the archive for the
host's platform against the size and SHA-256 the manifest gives, and
unpacked with `tar` (Windows 10's `tar.exe` reads the `.zip`). The new
binary must then answer `<binary> version` with exactly `<version> build
<n>` within 30 s, else the attempt fails (`the new <binary> does not start:
...`) and nothing is replaced. Only then it replaces itself by a rename,
keeping the replaced file as `<binary>.previous` (`Zeughaus.app.previous` on
macOS), and the editor installed beside it (runner only): `zeughaus` in the
runner's directory on Linux and Windows, `~/Applications/Zeughaus.app` on
macOS, if there is one. The CI replaces `zeughaus-ci` only and restarts by
`exec` of its own path with the same arguments. Every running editor of that
copy gets `SIGUSR1` (Linux: a process whose `exe` is that file or
`(deleted)` and that catches it; macOS: `pgrep -f` on the app's binary;
Windows: none, editors keep their build until reopened), and the runner
restarts the way its own `SIGUSR1` does. On Windows the runner updates only
while idle (not held, no live run) and restarts by exiting, which the boot
task's `runner.cmd` loop answers by starting the new binary. A failed
attempt is logged as `[update] build <n>: <error>` and not tried again by
that process.

A new build is on trial. `<state-dir>/updates/state.json` records
`installed`, `previous`, `starts`, `confirmed`, `refreshed` and `refused`.
Each start of an installed, unconfirmed build counts; after 120 s of running
it is confirmed (`[update] build <n> confirmed`). The 4th start of an
unconfirmed build puts the `.previous` files back, refuses that build until a
newer one is published, logs `[update] build <n> did not come up 3 times; back
to build <m>` and `exec`s the old binary (Windows: exits, and `runner.cmd`
restarts it). `zeughaus-runner [--state-dir D] update status|rollback` and
`zeughaus-ci [--state-dir D] update status|rollback` read and undo that:
`status` prints `following <channel> at <url>` (or `not following a channel
(no update.toml)`), `running build <own> (<version>)`, `installed <n>,
previous <m>, confirmed|unconfirmed, <k> starts` and `refused <n|->`;
`rollback` restores `.previous`, records it the same way and does not
restart, answering `rolled back to build <m>; restart the service to run it:
<hint>` with the hint of the host's profile from `install.toml`. The first
start of each newly installed build on a host with `install.toml` also
rewrites that profile's files (units, desktop entry, icon, VM scripts,
`maintain.toml`) from `<state-dir>/updates/<build>/staging/`, which the
updater keeps for the installed and the previous build; a changed unit gets
`daemon-reload` and applies at the next start of the service, and binaries
and service are not touched. `tuco-pc-hannes`, atik, the `tuco-pc` service
runner, sadala (runner and CI) and wohnzimmer all have `channel = "dev"`.

`zeughaus-runner release keygen <file>|bundle|verify <dir>` makes the
signing key (seed in a 0600 file, public half printed for `TRUSTED_KEYS`),
bundles the platform archives into a signed release directory
(`manifest.json`, `manifest.sig`, `SHA256SUMS`; the seed comes from
`ZEUGHAUS_UPDATE_KEY`, never from an argument) and checks one.

This repository is one of its repos: a push to `main`, a `v*` tag and the
nightly cron (`0 3 * * *`) run a build job and a check job per platform,
against weida's `main` as the sibling checkout, and post `zeughaus/<job>`
on the commit; pushes to other branches start nothing. `build-<platform>`
packs `zeughaus-<version>-<platform>.tar.gz` (`.zip` on Windows) into its
output, and `check-<platform>` runs clippy and the tests after it in the
same warm workspace (the unit tier on a push, both tiers on the nightly cron
and a tag; `check-linux-amd64` also runs `fmt`): `linux-amd64` (tuco-pc,
in the `.zeughaus-ci/trixie.Containerfile` image), `linux-arm64` (atik, the same
image in an Apple `container`), `windows-amd64` (the `wohnzimmer` runner's
host), `darwin-arm64` (atik's host; the editor as `Zeughaus.app` through
`deploy/macos-app.sh`). The `nightly` and `release` channels are `clean`:
their pipelines start from empty workspaces. `.zeughaus-ci/build.sh` (and
`windows.ps1` on Windows) is their shared helper. `publish.sh` runs on
sadala after all eight,
bundles and signs them (the `zeughaus-runner` of the `linux-arm64` archive is
the tool) and publishes into `/var/www/zeughaus-updates/<channel>/<build>/`
with `latest` naming the newest; the two newest builds of a channel stay.
Caddy serves that directory as
`https://zeughaus.doodleshnookie.net/updates/<channel>/{latest,<build>/}`
(`handle_path /updates/*` in the site block, `latest` uncached). The channel
follows the event: a push to `main` is `dev`, the cron `nightly`, a tag
`release`; the last two also become a GitHub release (`nightly-<date>` as a
prerelease, the seven newest kept; a tag `v<version>` with the CHANGELOG
section as its body, and it must equal the workspace version). Two secrets
in OpenBao `secret/zeughaus/ci` back it, granted to `push main`, `cron *`
and `tag v*`: `ZEUGHAUS_UPDATE_KEY` (the signing seed) and
`ZEUGHAUS_GITHUB_TOKEN` (contents write on `tuco86/zeughaus`).
Forgejo reaches the CI as the bot user `zeughaus-ci`
(write on zeughaus, read on weida); its tokens and the webhook secret are
in OpenBao under `secret/zeughaus/ci`. A change to `.zeughaus-ci/` is checked with
`zeughaus-ci check` before it is pushed. `DESIGN.md` section 14 describes how it
works.

### Agent stack and reload

An agent never drives the user's desktop. It builds its own binaries with
the `remote` feature into a separate target directory (so it neither
replaces the user's binaries nor holds their build lock), runs them against
its own state directory, and drives a headless editor over a
control socket:

```
CARGO_TARGET_DIR=target/agent cargo build -p zeughaus --features remote -p zeughaus-runner -p zeughaus-ci
ZEUGHAUS_STATE_DIR=/tmp/zh-agent target/agent/debug/zeughaus-runner
ZEUGHAUS_STATE_DIR=/tmp/zh-agent target/agent/debug/zeughaus --headless --control /tmp/zh-agent.sock
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
Signals to the agent's processes go by PID, never by name; an agent signals
only its own.

To test CI, the agent runs its own `zeughaus-ci --state-dir /tmp/zh-ci serve
--listen 127.0.0.1:7999` with a `ci.toml` that pins its agent runner's
fingerprint (from that runner's `endpoint` file), gives the runner an
`enlist.toml` (`ci` = the CI's `endpoint`, `name`), and lists the CI as `ci =
[..]` in the headless editor's `zeughaus.toml`. Never against the production
CI or its Forgejo webhook.

When a change is finished, its gate passed and it is committed, the agent
integrates it into the user's running stack by pushing to `main`: the CI
builds and publishes it to `dev`, and every host installs it within a minute
of the publish. After `ci_wait` shows the dev pipeline succeeded, the agent
confirms with `zeughaus-runner update status` on the host it can reach:
`installed <pipeline number>` at once and `confirmed` after 2 minutes; an
unconfirmed build that restarts three times is rolled back by the updater
itself. The agent does not install binaries or signal the user's processes,
and its own processes run from `target/agent`.

## Development Workflow

Plan, implement the minimal version, fix what you observe, refactor, commit,
push only after the gate:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run -p <crate>` for each crate the change touches: the unit
  tier, the only tests an agent runs locally

The pipelines run the rest: a push runs the unit tier and the doctests on
every platform, the nightly cron and a release tag also the integration
tier (`--profile ci`). Never run the whole suite, `--profile integration`
or `--profile ci` locally before a push. An I/O test lives in a `mod io`
inside its `#[cfg(test)] mod tests`; `.config/nextest.toml` holds the
tiers and their time limits.

`cargo publish --workspace --dry-run` belongs to the release (`RELEASING.md`
step 4), not to a change: it builds every crate again from its package and
took 7.6 min on 2026-10-18. A change that adds a crate or changes what a
crate includes checks that crate's file list with `cargo package -p <crate>
--no-verify --allow-dirty` (no compile).

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
- Anything that travels between processes has one definition: graph rows in
  `zeughaus-core::document`, runtime traffic in `zeughaus-link`, terminals
  in `zeughaus-mux`. Changing a wire type means changing both ends in one
  commit.
- The Rust toolchain is pinned in `rust-toolchain.toml`; bumping it also
  changes the `FROM` tag of `.zeughaus-ci/trixie.Containerfile`.
