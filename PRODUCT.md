# Product

<!-- impeccable:product-schema 1 -->

Facts marked *(inferred)* come from the repository and the owner's brief
("technically the CI is cool, the UX is still laughable"), not from an
interview; confirm or correct them.

## Platform

desktop

## Stack

iced 0.14 (Rust)

## Users

- The owner and a small circle of developers who build their own projects
  (zeughaus, weida, griasdi) on their own machines and a VPS. *(inferred)*
- They live in the editor for hours: terminals, node graphs and the CI view
  side by side, mostly keyboard driven, on 1920x1080 and laptop screens,
  Linux (Wayland) and macOS.

## Product Purpose

Zeughaus is a visual dataflow workbench and the place its users run and watch
their work: node graphs executed by headless runners, terminals multiplexed
by those runners, and a CI that schedules pipelines onto the same runners.
Success is that a user sees what runs where, what broke and why, and acts on
it without leaving the editor or reaching for a shell on another host.

## Positioning

One editor that is a remote view of every runner the user owns: graphs,
shells and CI live in the same tab tree, and a failed CI job is a terminal
to debug in on the machine where it ran, not a log on a web page.

## Operating Context

- Several runners (tuco-pc, sadala, atik) and one CI service, reached over
  WireGuard; editors on more than one machine at once.
- CI pipelines per repository and channel (`dev`, `nightly`, `release`,
  feature branches), jobs on hosts, containers and a Windows VM.
- The CI web surface does not exist: the editor's CI section is the only UI
  besides `zeughaus-ci status|log` on the CI host.

## Capabilities and Constraints

- The editor never executes; everything it shows is what a runner or the CI
  reported (see DESIGN.md, the architecture document).
- iced 0.14 has no accessibility tree; keyboard operation and contrast are
  what the app can give.
- The browser build edits locally and reaches no runner.

## Brand Commitments

- The name Zeughaus and the stencilled Z mark (`zeughaus/assets/icon`).
- Themes come from iced's built-in pack paired with terminal schemes, plus
  WezTerm schemes the user drops in; the editor follows the chosen theme,
  light or dark, and has no palette of its own.
- The bundled ComicShannsMono Nerd Font is the terminal face and the icon
  font (codicons) of the chrome.

## Product Principles

- Status first: what is red, what is running and why is readable at a glance.
- Every action has a place a keyboard user reaches; nothing is hover-only.
- Calm chrome: the work (graphs, terminals, logs) is the loudest thing on
  screen. *(inferred)*
- Show what the system knows, never invent a state the runner did not report.
