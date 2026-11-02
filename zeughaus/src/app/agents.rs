//! How the editor shows what an omp agent in a runner's terminal reports.

use iced::Color;
use zeughaus_link::{ActivityEntry, ActivityState, AgentRole, AgentState, AgentStatus};
use zeughaus_theme::Theme;

use super::App;
use crate::message::NoticeTarget;
use crate::notices::Severity;
use crate::workspace::RunnerKey;

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

/// The word of an agent's role.
fn role_word(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Work => "work",
        AgentRole::Plan => "plan",
        AgentRole::Master => "master",
    }
}

/// What an agent that needs its user is waiting for, as a line:
/// `zeughaus · master #33 waits for you`.
pub(super) fn attention_title(agent: &AgentStatus) -> String {
    let verb = match agent.state {
        AgentState::Asking => "asks",
        AgentState::Review => "wants a plan reviewed",
        AgentState::Failed => "failed",
        _ => "waits for you",
    };
    let doc = agent.doc.map(|n| format!(" #{n}")).unwrap_or_default();
    format!("{} · {}{doc} {verb}", agent.repo, role_word(agent.role))
}

/// The key of the notice of the agent in `terminal` of runner `key`.
fn notice_key(key: &RunnerKey, terminal: u64) -> String {
    format!("agent:{}:{terminal}", key.as_str())
}

impl App {
    /// Raises the notice of an agent that came to need its user (`held` is
    /// the state it had before) and nobody looks at, and resolves it once
    /// the agent works again, ends, or its terminal is gone.
    pub(super) fn agent_notice(
        &mut self,
        key: &RunnerKey,
        held: Option<AgentState>,
        entry: &ActivityEntry,
    ) {
        let notice = notice_key(key, entry.terminal);
        let agent = entry
            .agent
            .as_ref()
            .filter(|_| entry.state != ActivityState::Closed);
        match agent {
            Some(agent) if agent.needs_attention() => {
                let terminal = zeughaus_mux::TerminalId(entry.terminal);
                if held != Some(agent.state) && !self.watched(key, terminal) {
                    let severity = if agent.state == AgentState::Failed {
                        Severity::Error
                    } else {
                        Severity::Warning
                    };
                    self.raise(
                        Some(notice),
                        severity,
                        attention_title(agent),
                        agent.detail.clone(),
                        true,
                        Some(NoticeTarget::Terminal(key.clone(), terminal)),
                    );
                }
            }
            _ => self.notices.resolve(&notice),
        }
    }

    /// Resolves the notices of the agents the user now looks at: looking at
    /// one acknowledges it.
    pub(super) fn resolve_watched_agents(&mut self) {
        let watched: Vec<String> = self
            .runtime
            .agents()
            .filter(|(key, e)| self.watched(key, zeughaus_mux::TerminalId(e.terminal)))
            .map(|(key, e)| notice_key(key, e.terminal))
            .collect();
        for key in watched {
            self.notices.resolve(&key);
        }
    }
}
