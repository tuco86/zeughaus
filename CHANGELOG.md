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
  a repository defines as scripts in `.ci/` (TOML headers: `on`, `needs`,
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
- The workspace gate runs on the CI for every push (`.ci/gate.sh`) and
  reports `zeughaus/gate` on the commit in Forgejo.

### Changed

- The mux protocol is major version 3: a key carries press or release, and
  a notification is a terminal event. Editor and runner must be updated
  together.
- A shell keeps 20 000 rows of history in the runner; an editor caches 1 000
  rows per terminal around its viewport and fetches the rest on scroll.

### Fixed

- The editor no longer panics when a terminal switches between the primary
  and the alternate screen while scrolled back, or is scrolled or resized on
  the alternate screen after scrollback was evicted: the two screens number
  their rows independently, so each switch now starts a new terminal epoch
  and the runner sends a fresh head instead of a delta.
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
