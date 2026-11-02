---
version: 1
slug: "zeughaus-src-app-ci-page-rs"
primary_target: "zeughaus/src/app/ci/page.rs"
related_targets: ["zeughaus/src/app/mod.rs","zeughaus/src/app/projects.rs"]
---

# CI views: Overview, project Board, channel

Mode: Operate. The owner and a few developers, keyboard first, on 1920x1080
and laptop screens, light and dark themes.

Job: see what waits for approval, what runs where and whether dev, nightly
and release are green; approve or plan with one key; jump into the live
agent terminal of a running card. The Overview lists what needs the user,
what works, the projects to open and each CI's runners.

Constraints: iced 0.14, no accessibility tree; the editor's theme roles
only; the bundled ComicShannsMono Nerd Font Propo face and its codicon and
material glyphs.

## Direction contract

THESIS. A project's CI is its lifecycle on one screen, read left to right:
proposals become plans, plans queue, run and finish, with the channels'
health above. It refuses the category default of a repository sidebar over
pipeline logs.

OWN-WORLD. The editor's established world: theme roles through `Tokens`,
the ComicShannsMono Nerd Font Propo face and its codicon/material glyphs,
flat cards with hairline borders, status as glyph colour (`Tone`), host
symbols for machines; no shadows, no gradients.

STORY. The owner sees what waits for approval, what runs where and whether
dev/nightly/release are green. They approve or plan with one key, and jump
into the live agent terminal of a running card.

FIRST VIEWPORT. The project's Board tab: the channel rail across the top
(glyph, name, 12 pipeline dots per channel), below it five equal columns
PROPOSALS | PLANS | QUEUE | RUNNING | DONE with counts. RUNNING cards carry
host symbol and live agent state. The detail pane takes the right 4/9 when a
document is selected.

FORM. A lifecycle conveyor over the plan state machine, keyboard first
(h/l, j/k, Enter, a/r/p, Escape). Seed key: c2b7c987.

SIGNATURE. A card that the CI moves to another column arrives with an accent
ring that fades over 2 s: the conveyor visibly moves.
