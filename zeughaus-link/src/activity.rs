//! What a runner's own processes are doing: every terminal it owns (shells,
//! agent sessions, job runs, CI jobs, maintenance), live or archived under
//! `<state-dir>/runs/`.
//!
//! An editor asks [`ACTIVITY_PATH`] for the newest entries once and then
//! follows [`crate::events::RuntimeEvent::Activity`], which carries every
//! change of one entry. JSON, so a field added later is
//! `#[serde(default)]` and older peers keep reading.

use serde::{Deserialize, Serialize};

/// Req/Rep endpoint a runner answers [`ActivityRequest`]s on.
pub const ACTIVITY_PATH: &str = "/activity";

/// The most entries one reply carries.
pub const MAX_ACTIVITY_ENTRIES: u32 = 200;

/// The largest request a runner reads.
pub const MAX_ACTIVITY_REQUEST_BYTES: usize = 1024;

/// The largest reply an editor reads.
pub const MAX_ACTIVITY_REPLY_BYTES: usize = 1 << 20;

/// What a terminal of the runner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// A shell a pane opened.
    Shell,
    /// An agent session (plan or master).
    Session,
    /// A job node's run, started in the graph or by an editor's press.
    Job,
    /// A job node's run, pressed from outside any editor.
    Trigger,
    /// A job the CI assigned.
    CiJob,
    /// A task of `maintain.toml`.
    Maintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    /// The child runs.
    Running,
    /// The child ended; the terminal is still there to read.
    Exited,
    /// The terminal is gone; an archived one is under `runs/<run>/`.
    Closed,
}

/// One terminal of the runner, by its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub terminal: u64,
    /// The run directory under `<state-dir>/runs/` that holds its output.
    #[serde(default)]
    pub run: Option<u64>,
    pub kind: ActivityKind,
    pub label: String,
    pub state: ActivityState,
    /// Unix seconds.
    pub started: u64,
    #[serde(default)]
    pub ended: Option<u64>,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub killed: bool,
    /// What the omp agent in this terminal reports, while one does.
    #[serde(default)]
    pub agent: Option<AgentStatus>,
}

/// The terminal user variable (iTerm2's `OSC 1337;SetUserVar`) an omp agent
/// sets to report its [`AgentStatus`]: base64 of its JSON.
pub const AGENT_VAR: &str = "zeughaus_agent";

/// What an agent is doing, as the zeughaus omp plugin reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Started, no turn yet.
    Idle,
    /// A turn runs.
    Working,
    /// The turn ended; the agent waits for the next prompt.
    Waiting,
    /// The agent asked a question and waits for the answer.
    Asking,
    /// A plan waits for review.
    Review,
    /// The turn ended with an error.
    Failed,
    /// The agent quit: there is no status any more.
    Ended,
}

/// Which kind of omp session it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Someone's own session.
    #[default]
    Work,
    /// A plan session: a proposal becoming a plan.
    Plan,
    /// A master executing a plan.
    Master,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentStatus {
    pub state: AgentState,
    #[serde(default)]
    pub role: AgentRole,
    #[serde(default)]
    pub repo: String,
    /// The plan or proposal the session works on.
    #[serde(default)]
    pub doc: Option<u64>,
    #[serde(default)]
    pub detail: String,
    /// When the agent reported it, in milliseconds since the epoch.
    #[serde(default)]
    pub at: u64,
    /// The agent's process, to tell a dead one from a quiet one.
    #[serde(default)]
    pub pid: Option<u32>,
    /// The host the agent runs on, which `pid` is on.
    #[serde(default)]
    pub host: String,
}

/// The longest `detail` kept, in chars.
const MAX_AGENT_DETAIL: usize = 200;
/// The longest `repo` kept, in chars.
const MAX_AGENT_REPO: usize = 100;

/// The plugin's wire object: the status plus its version.
#[derive(Deserialize)]
struct AgentWire {
    v: u32,
    #[serde(flatten)]
    status: AgentStatus,
}

impl AgentStatus {
    /// The status of the plugin's JSON object (`"v": 1`). `None` for another
    /// version, malformed JSON, and an ended agent.
    pub fn parse(json: &str) -> Option<AgentStatus> {
        let wire: AgentWire = serde_json::from_str(json).ok()?;
        if wire.v != 1 || wire.status.state == AgentState::Ended {
            return None;
        }
        let mut status = wire.status;
        cut_chars(&mut status.detail, MAX_AGENT_DETAIL);
        cut_chars(&mut status.repo, MAX_AGENT_REPO);
        Some(status)
    }

    /// Whether the agent waits for its user.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self.state,
            AgentState::Waiting | AgentState::Asking | AgentState::Review | AgentState::Failed
        )
    }
}

/// Cuts `text` to at most `max` chars.
fn cut_chars(text: &mut String, max: usize) {
    if let Some((at, _)) = text.char_indices().nth(max) {
        text.truncate(at);
    }
}

/// The terminal user variable the zeughaus omp plugin sets when its user
/// picks a ticket link: base64 of JSON
/// `{"v":1,"repo":"weida","number":12,"at":<ms>}`.
pub const SHOW_VAR: &str = "zeughaus_show";

/// How old a [`SHOW_VAR`] request may be, in milliseconds: a replayed or
/// stale value must not reopen a popout.
const MAX_SHOW_AGE_MS: u64 = 30_000;

/// A request to show ticket `number` of the CI repository `repo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowTicket {
    pub repo: String,
    pub number: u64,
}

/// The plugin's wire object for [`SHOW_VAR`].
#[derive(Deserialize)]
struct ShowWire {
    v: u32,
    repo: String,
    number: u64,
    at: u64,
}

impl ShowTicket {
    /// The request of the plugin's JSON object, `None` for another version,
    /// a repository that is no CI name, ticket 0, malformed JSON, and a
    /// request made more than 30 s away from `now_ms`.
    pub fn parse(json: &str, now_ms: u64) -> Option<ShowTicket> {
        let wire: ShowWire = serde_json::from_str(json).ok()?;
        if wire.v != 1
            || !crate::ticket_text::repo_name(&wire.repo)
            || wire.repo.len() > MAX_AGENT_REPO
            || wire.number == 0
            || now_ms.abs_diff(wire.at) > MAX_SHOW_AGE_MS
        {
            return None;
        }
        Some(ShowTicket {
            repo: wire.repo,
            number: wire.number,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRequest {
    /// The most entries wanted, capped at [`MAX_ACTIVITY_ENTRIES`].
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityReply {
    /// Newest first.
    pub entries: Vec<ActivityEntry>,
}

impl ActivityRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than
    /// [`MAX_ACTIVITY_REQUEST_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_ACTIVITY_REQUEST_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

impl ActivityReply {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than
    /// [`MAX_ACTIVITY_REPLY_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_ACTIVITY_REPLY_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_round_trips_and_older_entries_still_read() {
        let reply = ActivityReply {
            entries: vec![ActivityEntry {
                terminal: 7,
                run: Some(12),
                kind: ActivityKind::CiJob,
                label: "zeughaus/check #3".to_owned(),
                state: ActivityState::Closed,
                started: 100,
                ended: Some(160),
                code: Some(0),
                killed: false,
                agent: None,
            }],
        };
        assert_eq!(ActivityReply::decode(&reply.encode()), Some(reply));
        let minimal = br#"{"entries":[{"terminal":1,"kind":"shell","label":"bash","state":"running","started":5}]}"#;
        let decoded = ActivityReply::decode(minimal).expect("decodes");
        assert_eq!(decoded.entries[0].kind, ActivityKind::Shell);
        assert_eq!(decoded.entries[0].run, None);
        assert_eq!(decoded.entries[0].agent, None);
        assert_eq!(
            ActivityRequest::decode(&ActivityRequest { limit: 20 }.encode()),
            Some(ActivityRequest { limit: 20 })
        );
        assert_eq!(ActivityRequest::decode(b"nope"), None);
    }

    #[test]
    fn an_agent_status_parses_from_the_plugins_object() {
        let json = r#"{"v":1,"state":"asking","role":"master","repo":"zeughaus","doc":33,"detail":"Which way?","at":1700000000000,"pid":42,"host":"atik"}"#;
        assert_eq!(
            AgentStatus::parse(json),
            Some(AgentStatus {
                state: AgentState::Asking,
                role: AgentRole::Master,
                repo: "zeughaus".to_owned(),
                doc: Some(33),
                detail: "Which way?".to_owned(),
                at: 1_700_000_000_000,
                pid: Some(42),
                host: "atik".to_owned(),
            })
        );
        assert!(AgentStatus::parse(json).unwrap().needs_attention());
    }

    #[test]
    fn another_version_an_ended_agent_and_garbage_give_no_status() {
        assert_eq!(AgentStatus::parse(r#"{"v":2,"state":"working"}"#), None);
        assert_eq!(AgentStatus::parse(r#"{"v":1,"state":"ended"}"#), None);
        assert_eq!(AgentStatus::parse(r#"{"state":"working"}"#), None);
        assert_eq!(AgentStatus::parse("{"), None);
        let working = AgentStatus::parse(r#"{"v":1,"state":"working"}"#).unwrap();
        assert_eq!(working.role, AgentRole::Work);
        assert!(!working.needs_attention());
    }

    #[test]
    fn a_long_detail_is_cut_to_200_chars() {
        let detail = "\u{e4}".repeat(300);
        let json = format!(r#"{{"v":1,"state":"failed","detail":"{detail}"}}"#);
        let status = AgentStatus::parse(&json).unwrap();
        assert_eq!(status.detail.chars().count(), 200);
    }

    #[test]
    fn a_fresh_show_request_parses_and_anything_else_does_not() {
        let now = 1_700_000_040_000;
        let show = |v: u32, repo: &str, number: u64, at: u64| {
            format!(r#"{{"v":{v},"repo":"{repo}","number":{number},"at":{at}}}"#)
        };
        assert_eq!(
            ShowTicket::parse(&show(1, "weida", 12, now - 1_000), now),
            Some(ShowTicket {
                repo: "weida".to_owned(),
                number: 12,
            })
        );
        assert_eq!(
            ShowTicket::parse(&show(1, "weida", 12, now - 31_000), now),
            None
        );
        assert_eq!(ShowTicket::parse(&show(2, "weida", 12, now), now), None);
        assert_eq!(ShowTicket::parse(&show(1, "Weida", 12, now), now), None);
        assert_eq!(ShowTicket::parse(&show(1, "../x", 12, now), now), None);
        assert_eq!(ShowTicket::parse(&show(1, "weida", 0, now), now), None);
        assert_eq!(ShowTicket::parse(r#"{"v":1,"repo":"weida""#, now), None);
    }
}
