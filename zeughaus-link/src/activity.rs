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
            }],
        };
        assert_eq!(ActivityReply::decode(&reply.encode()), Some(reply));
        let minimal = br#"{"entries":[{"terminal":1,"kind":"shell","label":"bash","state":"running","started":5}]}"#;
        let decoded = ActivityReply::decode(minimal).expect("decodes");
        assert_eq!(decoded.entries[0].kind, ActivityKind::Shell);
        assert_eq!(decoded.entries[0].run, None);
        assert_eq!(
            ActivityRequest::decode(&ActivityRequest { limit: 20 }.encode()),
            Some(ActivityRequest { limit: 20 })
        );
        assert_eq!(ActivityRequest::decode(b"nope"), None);
    }
}
