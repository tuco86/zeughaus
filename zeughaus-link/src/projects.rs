//! The projects a runner's user can open: every repository the forges of
//! `[projects]` in the runner's `zeughaus.toml` give push access to, plus
//! every checkout under its work root; and on request what the runner can
//! find out about one of them, for the CI's advice.
//!
//! An editor asks its local runner on [`PROJECTS_PATH`], which answers from
//! a cache unless [`ProjectsRequest::refresh`] is set. JSON, so a field
//! added later is `#[serde(default)]` and older peers keep reading.

use serde::{Deserialize, Serialize};

/// Req/Rep endpoint a runner answers [`ProjectsRequest`]s on.
pub const PROJECTS_PATH: &str = "/projects";

/// The largest request a runner reads.
pub const MAX_PROJECTS_REQUEST_BYTES: usize = 1024;

/// The largest reply an editor reads.
pub const MAX_PROJECTS_REPLY_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectsRequest {
    /// Ask the forges again instead of answering from the cache.
    #[serde(default)]
    pub refresh: bool,
    /// An origin: answer [`ProjectsReply::facts`] about it and no listing.
    #[serde(default)]
    pub facts: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectsReply {
    /// Sorted by name, then origin.
    pub projects: Vec<ProjectView>,
    /// One line per forge that could not be listed, or the configuration
    /// that could not be read. The rest is still listed.
    #[serde(default)]
    pub errors: Vec<String>,
    /// What the runner found out about the asked origin, in Markdown: its
    /// listing, its work checkout (files, manifests, README, remotes) or
    /// else what its forge says. At most
    /// [`crate::advice::MAX_FACTS_BYTES`].
    #[serde(default)]
    pub facts: Option<String>,
}

/// One repository the user can push to, or has checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    /// As [`crate::normalize_origin`] gives it: `host/owner/name`.
    pub origin: String,
    pub name: String,
    /// Where it was listed: `forgejo`, `github` or `local`.
    pub forge: String,
    #[serde(default = "main_branch")]
    pub default_branch: String,
    /// Whether the work checkout `<work root>/<origin>` exists.
    #[serde(default)]
    pub checkout: bool,
}

fn main_branch() -> String {
    "main".to_owned()
}

impl ProjectsRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than
    /// [`MAX_PROJECTS_REQUEST_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_PROJECTS_REQUEST_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

impl ProjectsReply {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than
    /// [`MAX_PROJECTS_REPLY_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_PROJECTS_REPLY_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_reply_reads_with_its_defaults() {
        let reply = ProjectsReply::decode(
            br#"{"projects":[{"origin":"h/o/r","name":"r","forge":"local"}]}"#,
        )
        .unwrap();
        assert_eq!(reply.projects[0].default_branch, "main");
        assert!(!reply.projects[0].checkout);
        assert!(reply.errors.is_empty());
        assert_eq!(
            ProjectsRequest::decode(b"{}"),
            Some(ProjectsRequest::default())
        );
    }
}
