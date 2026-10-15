# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Terminals speak kitty's keyboard protocol to a child that asks for it
  (Shift+Enter, Ctrl+letters and key releases are unambiguous), negotiated
  in-band so it works through ssh. Delete stays `CSI 3 ~` under it, not
  the Backspace byte WezTerm's encoder sends.
- OSC 9 and OSC 777 notifications from a terminal appear on the desktop,
  unless its pane is focused in the focused window.
- Ctrl+click on an OSC 8 link opens it with the platform's handler
  (`http`, `https` and `file` links only), also while the child reads the
  mouse.
- The wheel scrolls programs on the alternate screen that do not read the
  mouse (`less`, `man`), by turning into cursor keys as in xterm.
- `zeughaus ctl ... click X Y [BUTTON] [MODS]` holds modifiers across a
  click.
- `Ctrl+PageDown` and `Ctrl+PageUp` switch to the next and the previous tab
  across every runner's section, wrapping at the ends, also while a
  terminal has the keyboard; its child (vim, tmux) no longer receives them.
  Not while the palette or a rename field is open.
- CI: an editor can rerun a pipeline (its event again, pinned to the same
  commit) and cancel a running one (jobs killed or skipped, the pipeline
  ends as `cancelled`, no red alert); a failed job's cause line and log
  excerpt are part of the pipeline view.
- CI: the service `zeughaus-ci` runs pipelines that
  a repository defines as scripts in `.zeughaus-ci/` (TOML headers: `on`, `needs`,
  `image`, `machine`, `platform`, `cache`, `secrets`, `when_busy`, `env`,
  `timeout_minutes`) on the runners that enlist into it:
  - jobs run on the host, in rootless podman containers (Apple `container`
    on macOS), or in a managed Windows VM;
  - pushes to a ref coalesce;
  - artifacts pass between jobs;
  - commit statuses go to GitHub or Forgejo;
  - expensive jobs wait or freeze while the GPU is busy;
  - a failed job's terminal becomes a shell where it ran.

  Subcommands: `zeughaus-ci serve|check|plan|run|status|log|forge-check|secrets-check`.
  `deploy/install-sadala.sh` installs it with a runner on sadala, from
  atik, where `deploy/build-aarch64.sh` builds both for aarch64 Linux in a
  native arm64 container. Editors list CIs as `ci = [..]` in `zeughaus.toml`.
- `zeughaus-runner timeout <seconds> -- <program> [args..]` runs a program
  under a deadline (`SIGTERM` to its process group, `SIGKILL` 60 s later,
  exit 124), because macOS has no `timeout`; host CI jobs use it.
- `vm/win11/vm.sh toolchain` installs the build toolchain into the golden
  image, and a persistent cache disk is attached as `W:`.
- The workspace gate runs on the CI for every push (`.zeughaus-ci/gate.sh`) and
  reports `zeughaus/gate` on the commit in Forgejo.
- The section of a runner that measures its GPU shows whether it counts as
  busy (a play or pause circle, filled when overridden) and cycles the busy
  mode on click: `auto` follows the GPU measurement, `busy` and `free`
  override it until set back. The mode survives a restart
  (`<state-dir>/ci/busy-mode`). New `/busy` endpoint and `machine` event;
  editor and runner must be updated together for the control to appear.
- `deploy/macos-app.sh` builds `Zeughaus.app` around a release build of the
  editor: the Info.plist that names the app, and an icns `render.py` draws
  at the ten sizes the Dock, the switcher and Finder ask for.
- A CI failure says why and a red default branch makes noise. A failed
  job keeps the 40 log lines before its exit (`<run>/excerpt`); its
  commit status names the cause line (`panicked at ...`) instead of the
  exit code, prefixed `red since #<n>:` while the default branch stays
  red. `zeughaus-ci status` leads with each default branch's streak and
  prints the excerpt of every failed job of the newest pipeline;
  `zeughaus-ci log <repo> <n> <job> [--tail N]` prints a job's log as
  plain text. A job turning the default branch red, and every red
  pipeline after its first hour, notify every connected editor's desktop
  (new `ci` event: editor and runner must be updated together). A push to
  the default branch without `.zeughaus-ci/` is recorded as `no-jobs` and
  posts an error on `zeughaus/pipeline`.
- CI placement by platform: `[platforms]` in `ci.toml` maps a platform name
  (`linux-amd64`, `darwin-arm64`, ...) to a runner or a Windows VM of one,
  and a job's `platform` header key (exclusive with `machine`) runs it there;
  with an `image` it runs in a container on that runner instead of the
  default runner. `when_busy = "freeze"` on a placement that resolves to a
  host is refused when the pipeline is set up. Jobs see `CI_PLATFORM`.
- A runner on macOS runs container jobs in Apple's `container` (Homebrew
  formula `container`) instead of podman, started on demand by the first job.
  Its inputs are mounted writable and a job in it is not frozen while the
  GPU is busy (the runner logs that it cannot).
- `zeughaus-runner release keygen|bundle|verify` makes and checks signed
  release bundles: per platform one `zeughaus-<version>-<platform>.tar.gz`
  (`.zip` on Windows), a `manifest.json` and its Ed25519 signature
  `manifest.sig`. The public key is compiled into the binary; the seed
  comes from `ZEUGHAUS_UPDATE_KEY`.
- A runner updates itself: with `<state-dir>/update.toml` (`channel`, and
  `url`, default `https://zeughaus.doodleshnookie.net/updates`) it checks
  the channel once a minute, and installs a build newer than its own after
  verifying signature, size and SHA-256. It replaces itself and the editor
  beside it (`zeughaus` next to it on Linux, `~/Applications/Zeughaus.app`
  on macOS; never `zeughaus-ci`), sends running editors `SIGUSR1` and
  restarts itself the way its own `SIGUSR1` does. Without the file nothing
  changes. Runners that carry the CI (sadala and the service runner on
  tuco-pc) have none, so a broken build cannot take down what would publish
  its fix. Unix only.
- zeughaus's own pipeline builds every platform and publishes it:
  `.zeughaus-ci/` has one build job per platform (`linux-amd64` on tuco-pc,
  `linux-arm64` in an Apple `container` on atik, `windows-amd64` in the
  Windows VM, `darwin-arm64` and `darwin-amd64` on atik's host) and
  `publish.sh` on sadala, which signs the five archives into a release. It
  goes to `https://zeughaus.doodleshnookie.net/updates/<channel>/` and, for
  the nightly cron and `v*` tags, to a GitHub release (`nightly-<date>` as a
  prerelease, seven kept). Channels `dev` (push to `main`), `nightly` and
  `release` (tag) are in `.zeughaus-ci/channels.toml`; two secrets,
  `ZEUGHAUS_UPDATE_KEY` and `ZEUGHAUS_GITHUB_TOKEN`, live in OpenBao
  `secret/zeughaus/ci`. This replaces the macOS job that zipped
  `Zeughaus.app` for Apple silicon. `deploy/macos-app.sh` honours
  `CARGO_TARGET_DIR` and takes an optional target triple. The runner builds
  on Windows; it neither enlists there (an `enlist.toml` is reported and
  ignored) nor offers `timeout`.
- A CI gets a `CI <label>` section in the tab bar, one for each URL under `ci`
  in `zeughaus.toml`: a `Runners` tab
  with a table of the enlisted runners, the Windows VM, their
  coloured state and their running jobs (each opens its transcript), and
  a locked group per repository with one tab per channel, its dot coloured
  by the channel's newest pipeline. A channel tab lists its pipelines in a
  plain scrollable, newest on top: what started each (`#200 push to main
  abcdef0 by cebor`), its status, when it started (`3h ago`, the exact time
  on hover; running pipelines and jobs count up live) and commit subject,
  and its jobs as a DAG of cards with cables drawn by the node graph's
  renderer; pressing a job shows its transcript read-only in the terminal
  widget, live while it runs. The busy control stays on the section of the
  runner that measures it. New `/ci` endpoint and `ci` pipeline event: editor
  and CI must be updated together. The CI fills in the commit
  subject of records from before it was kept once at start.
- Channels: `.zeughaus-ci/channels.toml` lists `[[channel]]` tables of
  `name` and `on` patterns; the first match names an event's channel, else
  the branch, `tags` or `cron`. `zeughaus-ci check|plan|status` show
  it. Job transcripts are kept on the CI with their pipeline record.
- Every terminal writes a WAL of its output with a timestamp per chunk:
  job runs to `<run>/wal`, shells to `wal` in their shim directory
  (rotated at 64 MiB, removed with the terminal).

### Changed

- CI secrets come from OpenBao instead of files under
  `<state-dir>/secrets/`: `[secrets]` in `ci.toml` names the KV v2
  document, the CI logs in with the AppRole `zeughaus-ci` whose
  secret_id `deploy/ci/openbao.sh` seals with `systemd-creds`, and reads
  at use time, so a rotated token takes effect without a restart. A repo
  that names secrets needs `[secrets]`. While OpenBao is unreachable the
  hook answers 503 and the forge redelivers. New `zeughaus-ci secrets-check`
  lists every named secret as `ok` or `missing`.
- A job run's output is `<run>/wal` instead of the raw `<run>/log`;
  `/runs` serves `wal` instead of `log`. Runs from before keep their `log`,
  and `zeughaus-ci log` and the failure excerpts read both.
- The CI keeps the newest 200 pipelines per channel instead of per
  repository, and removes a pruned pipeline's transcripts with it.
- Runners enlist into the CI. A runner with `<state-dir>/enlist.toml` (`ci`,
  `name`, `cpus`, `budget_gb`, and the tables `[containers]`, `[busy]` and
  `[machines.<name>]`, which moved there from `ci.toml`) dials it on the new
  `/enlist` path, executes the jobs it is assigned as its own runs and streams
  their logs to the CI, which stores them; artifacts between jobs on
  different runners go through the CI on `/artifacts`. `ci.toml` lists the
  runners that may enlist: `default_runner` and `[runners.<name>]`
  (`fingerprint`, `machines`, `wait_minutes`), and drops `budget_gb`, `[busy]`,
  `[containers]` and `[machines.*]`. `unix-host` machines are replaced by
  enlisted runners: `machine = "atik"` runs on the runner `atik`, a launchd
  agent installed by `deploy/install-macos.sh`, and a job whose runner
  is offline for `wait_minutes` is skipped, not failed. Editor, runner and CI
  must be updated together.
- The mux protocol is major version 3: a key carries press or release, and
  a notification is a terminal event. Editor and runner must be updated
  together.
- A shell keeps 20 000 rows of history in the runner; an editor caches 1 000
  rows per terminal around its viewport and fetches the rest on scroll.
- A section header's `+ Shell`, `+ Graph` and `+ Group` are glyphs from the
  bundled Nerd Font with tooltips; the browser editor, which does not
  bundle the font, keeps the words. `iced_tabs` header controls are
  `Control`s with an optional font and tooltip.
- Each runner holds its own graphs: it loads them from
  `<state-dir>/graphs/<id>.zgh` (one file per top-level graph), serves them
  to editors on the new `/graph` link path, applies their edits and writes
  the files back at most once per second and on restart and stop. Editor
  and runner must be updated together.
- The editor connects to the runner of its state directory through
  `<state-dir>/endpoint`, which a runner writes at start, and to every URL
  in `remotes = [..]` of `<state-dir>/zeughaus.toml`; both files are
  re-read when they change.
- Save exports the focused graph; Load imports a file as a new graph with
  fresh ids into the runner of the section in front.
- The CI is a separate binary, `zeughaus-ci`, and no longer a thread of the
  runner. It listens on `10.8.0.1:7444` on sadala next to its webhook intake
  (`serve --listen <host:port> [--hook <host:port>]`), and runners and
  editors dial it. The service runner of the workstation is the Unix user
  `zeughaus` (formerly `zeughaus-ci`), installed by
  `deploy/install-runner-user.sh`.
- CI artifacts are not retained: a set lives on the runner that made it
  until the last job that needs it has ended, then the CI tells that runner
  to delete it (`DropArtifacts`, and at each enlist for sets no pending job
  needs). A job on another runner gets the set relayed through the CI,
  which streams it without a copy on its disk and makes the job wait for the
  producing runner like for its own. The consumer's `<run>/inputs` goes when
  its run ends, or when the debug shell of a failed run exits, and a
  Windows VM's guest run directory with it. `KEEP_ARTIFACTS` on both sides
  is gone. The enlist protocol is major 2: CI and every runner must be
  updated together. Old `ci/pipelines/<repo>/<n>.artifacts/` directories on
  the CI are no longer read or pruned and can be deleted by hand.
- `zeughaus-ci check` places every job against the state directory's
  `ci.toml` when there is one, so `when_busy = "freeze"` on a host job (a
  `platform` or `machine` that is a runner) is refused there and not only
  when a pipeline is set up; without `ci.toml` it warns about each `freeze`
  job without an `image`.
- A CI channel tab is a list of pipelines, one row each: status glyph, number
  and commit subject, branch, commit and actor, each failed job's cause line,
  duration and age, and while closed one glyph per job. The newest and the
  running pipelines are open with their jobs as compact cards, `Rerun` and
  `Cancel`, and every failed job's log excerpt with `Open log`; a press on a
  row opens or closes it. The card whose log is open is tinted, waiting jobs
  are amber. Channel tabs are titled with the channel alone, the Runners tab
  shows a kind glyph, a state dot and `n of m online`. Status in the CI views
  is a codicon glyph as well as a colour.
- The command palette has two-line rows of one height (title, then the
  description), a full-window scrim and a soft shadow, and says when nothing
  matches; the status bar drops its pipes, counts the focused graph's nodes
  and edges only while a graph is focused, and gets its plurals right; an
  empty graph says how to add a node.
- Runner sections are labelled with the host alone, adding the port (or the
  start of the key) only where two runners share one host. The tab tree's
  chevrons and close buttons are codicons, and a locked group no longer
  says `locked`.
- Secondary text keeps at least 4.5:1 contrast in every theme: in light
  themes it was as faint as 2:1.

### Removed

- SpacetimeDB: the `zeughaus-sync` and `zeughaus-module` crates, the
  `zeughaus-store` unit, `join <host/database>` on both binaries and the
  palette's "copy session token". Graphs stored there are not migrated.
- `zeughaus-runner ci check|plan|run|status|log|forge-check|secrets-check|hook|replay`:
  the CI is `zeughaus-ci`, and a transcript is a fed terminal session on its
  own `/mux` instead of a `ci replay` terminal of a runner. A runner no
  longer has `ci.toml`, pipeline queues and records, the webhook
  intake, `/ci` or a scheduler; `deploy/install-ci.sh` and the
  `deploy/ci/zeughaus-ci-runner.service`/`zeughaus-ci-hook.service` units
  are gone.

### Fixed

- With the tab bar on top, tabs that do not fit are reachable: a thin
  scrollbar shows the overflow, the wheel scrolls the bar sideways, and
  the bar scrolls the active tab into view whenever it changes (also on
  `Ctrl+PageDown`/`Ctrl+PageUp`).
- systemd-oomd no longer takes every shell down with the runner: under a
  systemd user manager the runner starts each terminal's shim in a
  transient scope of its own (`systemd-run --user --scope`), so an OOM
  kill hits the one terminal using the memory. Shims started before the
  update stay in the runner's unit until they exit.
- The editor no longer panics when a terminal switches between the primary
  and the alternate screen while scrolled back, or is scrolled or resized on
  the alternate screen after scrollback was evicted: the two screens number
  their rows independently, so each switch now starts a new terminal epoch
  and the runner sends a fresh head instead of a delta.
- The editor shows its icon on macOS: an app reads it from the `Info.plist`
  of its bundle, so a binary run outside one (`cargo run`) now hands the
  mark to AppKit itself, once the window is open.
- Shift+Enter in a terminal sends a line feed instead of the carriage
  return of Enter, so a prompt such as omp's or Claude Code's breaks the
  line instead of submitting.
- A terminal pane that grows because a split was dissolved (a pane dragged
  out into the tab bar) or because its tab came to the front now resizes
  the terminal at once, instead of only at the next window resize: the pane
  compares its grid with the size the runner reports, not with a memory
  iced may have handed over from another pane.
- A runner whose endpoint cannot be bound (a `--feed-addr` on WireGuard
  before `wg0` is up at boot) exits and is restarted by its unit until the
  address exists, instead of running on without an endpoint: it showed as
  connected in the editor while owning no graph and no tab section.
- A runner refuses to start on a state directory another runner serves
  (`<state-dir>/runner.lock`): it reattached the first runner's shims, took
  their terminals from it and left them unreachable when it exited.
- `zeughaus-runner --help` and unknown arguments print the usage instead
  of starting a runner.
- A failed run's terminal leaves the `Triggered` group for the detached
  list (attach it from the palette to look) and closes when the shell that
  followed the failure ends, also for runs a previous runner recorded; CI
  debug shells no longer sit in the tab bar.
- Two jobs on one runner that need the same job's artifacts no longer
  download into the same temporary tar, where the first to finish removed
  the other's file.
- An OSC 8 link whose URI contains `;` opens whole instead of cut at the
  first `;`: `third_party/` follows wezterm to e2a98e1b, which also fixes
  `zeughaus-wezterm-escape-parser` (now 0.1.1) building without `std`.

## [0.1.0-alpha.1] - 2026-09-26

First release on crates.io, licensed MIT OR Apache-2.0.

### Added

- The editor (`zeughaus`) and the runner (`zeughaus-runner`), with the crates
  they share: `zeughaus-core`, `zeughaus-runtime`, `zeughaus-sync`,
  `zeughaus-link`, `zeughaus-mux`, `zeughaus-terminal`, `zeughaus-theme`,
  `iced_tabs` and `iced_terminal`.
- The plugins `zeughaus-transform`, `zeughaus-flow`, `zeughaus-graph`,
  `zeughaus-ml`, `zeughaus-llm`, `zeughaus-capture`, `zeughaus-db`,
  `zeughaus-record` and `zeughaus-job`.
- `third_party/`: the wezterm crates the terminal engine is built on, at the
  pinned wezterm revision, published as `zeughaus-wezterm-term`,
  `zeughaus-termwiz` and the other `zeughaus-*` packages they need.

### Changed

- `iced_tabs` depends on `iced_widget` instead of the `iced` umbrella crate,
  so it builds on its own without a windowing platform feature.
- `weida`, `iced_palette` and the wezterm crates resolve from crates.io
  releases instead of git revisions.

[Unreleased]: https://github.com/tuco86/zeughaus/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.1]: https://github.com/tuco86/zeughaus/releases/tag/v0.1.0-alpha.1
