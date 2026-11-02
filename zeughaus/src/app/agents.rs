//! How the editor shows what an omp agent in a runner's terminal reports.

use iced::Color;
use zeughaus_link::{AgentRole, AgentState};

use zeughaus_theme::Theme;

/// The glyph of an agent's role: a person for someone's own session, a
/// lightbulb for a plan session, a rocket for a master.
pub(super) fn role_glyph(role: AgentRole) -> char {
    match role {
        AgentRole::Work => '\u{ea67}',
        AgentRole::Plan => '\u{ea61}',
        AgentRole::Master => '\u{eb44}',
    }
}

/// The colour of an agent's state: accent while it works, the hint colour
/// while it waits for its user, the error colour after a failed turn.
pub(super) fn agent_color(state: AgentState, theme: &Theme) -> Color {
    match state {
        AgentState::Working => theme.accent(),
        AgentState::Waiting | AgentState::Asking | AgentState::Review => theme.hint(),
        AgentState::Failed => theme.error(),
        AgentState::Idle | AgentState::Ended => theme.muted(),
    }
}
