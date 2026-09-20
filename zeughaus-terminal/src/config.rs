//! What the runner tells the terminal core about itself.
//!
//! `wezterm-term` asks its embedder for everything that is policy rather than
//! protocol: how much scrollback to keep, which palette a fresh terminal
//! starts from, which Unicode version decides how wide a grapheme is. In
//! WezTerm that is a Lua configuration file; here it is a runner decision and
//! a handful of constants, because a client never gets to choose them (see
//! `TERMINAL_MUX_ARCHITECTURE_PLAN.md`, "Security": profile policy is the
//! runner's).
//!
//! Everything not answered here keeps the trait's default, and the defaults
//! that matter are deliberate: no kitty graphics, no kitty keyboard, no title
//! reporting (a program that can read the title back can exfiltrate it), no
//! CSI-u encoding.

use std::sync::Arc;

use wezterm_term::TerminalConfiguration;
use wezterm_term::color::ColorPalette;
use wezterm_term::config::NewlineCanon;
use wezterm_term::{LATEST_UNICODE_VERSION, UnicodeVersion};

/// Largest scrollback a profile may ask for. Each row is a `Line` that holds
/// at least its cells, so an unbounded number here is an unbounded allocation
/// driven by whoever configures the profile.
pub const MAX_SCROLLBACK_ROWS: usize = 100_000;

/// The runner's terminal policy. One per session, never changed afterwards,
/// so [`TerminalConfiguration::generation`] stays at zero and the terminal's
/// caches are never invalidated for a configuration reason.
#[derive(Debug)]
pub(crate) struct Config {
    scrollback: usize,
}

impl Config {
    /// A configuration keeping `scrollback` rows of history, bounded by
    /// [`MAX_SCROLLBACK_ROWS`].
    pub(crate) fn new(scrollback: usize) -> Arc<Config> {
        Arc::new(Config {
            scrollback: scrollback.min(MAX_SCROLLBACK_ROWS),
        })
    }
}

impl TerminalConfiguration for Config {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    /// The palette a session starts with. The client resolves indexed colours
    /// itself and only needs the 16 ANSI entries plus the defaults, which it
    /// gets in every head and delta; this is where those come from until an
    /// OSC 4/10/11 changes them.
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    /// Unicode 14 rather than WezTerm's conservative 9. WezTerm defaults low
    /// because it cannot know which system fonts and which decade of ncurses
    /// the user's programs were built against; a Zeughaus terminal ships one
    /// bundled font and one renderer, so the current width rules are the ones
    /// both ends agree on.
    fn unicode_version(&self) -> UnicodeVersion {
        LATEST_UNICODE_VERSION
    }

    /// A paste is one line-ending convention by the time it reaches the
    /// child: a CRLF pasted into a shell would otherwise submit twice.
    fn canonicalize_pasted_newlines(&self) -> NewlineCanon {
        NewlineCanon::CarriageReturn
    }
}
