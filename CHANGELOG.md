# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Terminals speak kitty's keyboard protocol to a child that asks for it
  (Shift+Enter, Ctrl+letters and key releases are unambiguous), negotiated
  in-band so it works through ssh.
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
- CI on a runner: with `<state-dir>/ci.toml`, a runner runs pipelines that
  a repository defines as scripts in `.zeughaus-ci/` (TOML headers: `on`, `needs`,
  `image`, `machine`, `cache`, `secrets`, `when_busy`, `env`,
  `timeout_minutes`):
  - jobs run on the host, in rootless podman containers, or in a managed
    Windows VM;
  - pushes to a ref coalesce;
  - artifacts pass between jobs;
  - commit statuses go to GitHub or Forgejo;
  - expensive jobs wait or freeze while the GPU is busy;
  - a failed job's terminal becomes a shell where it ran.

  New subcommands: `zeughaus-runner ci check|plan|run|status|forge-check|hook`.
  `deploy/install-ci.sh` sets up the dedicated `zeughaus-ci` user and its
  units.
- `vm/win11/vm.sh toolchain` installs the build toolchain into the golden
  image, and a persistent cache disk is attached as `W:`.
- The workspace gate runs on the CI for every push (`.zeughaus-ci/gate.sh`) and
  reports `zeughaus/gate` on the commit in Forgejo.
- A CI runner's section in the tab bar shows whether its machine counts as
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

### Changed

- CI secrets come from OpenBao instead of files under
  `<state-dir>/secrets/`: `[secrets]` in `ci.toml` names the KV v2
  document, the runner logs in with the AppRole `zeughaus-ci` whose
  secret_id `deploy/ci/openbao.sh` seals with `systemd-creds`, and reads
  at use time, so a rotated token takes effect without a restart. A repo
  that names secrets needs `[secrets]`. While OpenBao is unreachable the
  hook answers 503 and the forge redelivers. New `ci secrets-check` lists
  every named secret as `ok` or `missing`.
- CI machines have a `kind`: `windows-vm` (the existing VM; `ci.toml`
  needs `kind = "windows-vm"` added) or `unix-host`, a host reached over
  ssh that is always on, such as a Mac. A unix host's jobs wait up to
  `wait_minutes` (15) for it to answer and are then skipped, not failed,
  so they never turn a branch red; they ignore the workstation's busy
  state. `deploy/install-ci.sh --update` now ships the CI user's ssh
  config, which names the Mac `atik-ci`.
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
- The CI runner listens on the fixed loopback port 7444, and
  `deploy/install-ci.sh` prints the `remotes` line for its URL.

### Removed

- SpacetimeDB: the `zeughaus-sync` and `zeughaus-module` crates, the
  `zeughaus-store` unit, `join <host/database>` on both binaries and the
  palette's "copy session token". Graphs stored there are not migrated.

### Fixed

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
