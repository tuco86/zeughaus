//! Agent sessions an editor opens on its own runner: a planning session in a
//! repository's `-plan` worktree, or the master session that executes the
//! approved plans of a repository one after another in its main checkout.
//!
//! One req/rep exchange on [`AGENT_PATH`](crate::AGENT_PATH): an
//! [`AgentRequest`] in, an [`AgentReply`] out, both JSON. The session runs in
//! a terminal of the runner's mux, which the editor then shows.

use serde::{Deserialize, Serialize};

/// Largest agent exchange either end reads: a few names.
pub const MAX_AGENT_BYTES: usize = 4096;

/// Which session to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSession {
    /// omp in plan mode in the repository's `<checkout>-plan` worktree.
    Plan,
    /// The loop that executes the repository's approved plans.
    Master,
}

/// Opens a session for one repository of the CI. `origin` is the
/// repository's url as `normalize_origin` gives it; the runner finds the
/// checkout under its work root by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub session: AgentSession,
    pub repo: String,
    pub origin: String,
    pub default_branch: String,
}

impl AgentRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than [`MAX_AGENT_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_AGENT_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

/// The terminal the session runs in, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentReply {
    /// `existing`: the session already ran and this is its terminal.
    Opened {
        terminal: u64,
        existing: bool,
    },
    Refused {
        message: String,
    },
}

impl AgentReply {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than [`MAX_AGENT_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_AGENT_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_exchange_round_trips() {
        let request = AgentRequest {
            session: AgentSession::Master,
            repo: "demo".to_owned(),
            origin: "git.example.org/o/demo".to_owned(),
            default_branch: "main".to_owned(),
        };
        assert_eq!(AgentRequest::decode(&request.encode()), Some(request));
        for reply in [
            AgentReply::Opened {
                terminal: 7,
                existing: true,
            },
            AgentReply::Refused {
                message: "no checkout at /x".to_owned(),
            },
        ] {
            assert_eq!(AgentReply::decode(&reply.encode()), Some(reply));
        }
    }

    #[test]
    fn an_oversized_agent_request_decodes_to_nothing() {
        let mut bytes = AgentRequest {
            session: AgentSession::Plan,
            repo: "demo".to_owned(),
            origin: "o".to_owned(),
            default_branch: "main".to_owned(),
        }
        .encode();
        bytes.resize(MAX_AGENT_BYTES + 1, b' ');
        assert!(AgentRequest::decode(&bytes).is_none());
    }
}
