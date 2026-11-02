//! What the CI advises for a project it does not build yet: what the
//! project is, where it stands, and the ways it could join, each with the
//! onboarding settings it would file.
//!
//! The editor gathers the facts from its local runner (`ProjectsRequest`
//! with `facts`) and hands them to the CI ([`crate::CiRequest::Advise`]),
//! which asks a language model and answers [`AdviceView`]. JSON with
//! defaults throughout: a field added later keeps older peers reading.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The most facts about one project an editor sends, in bytes.
pub const MAX_FACTS_BYTES: usize = 24 << 10;

/// The most operations one [`AdviceView`] carries.
pub const MAX_OPERATIONS: usize = 4;

/// The advice for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceView {
    /// As [`crate::normalize_origin`] gives it: `host/owner/name`.
    pub origin: String,
    /// What the project is, in two or three sentences.
    #[serde(default)]
    pub summary: String,
    /// Short facts about where it stands for the CI: its forge and
    /// mirrors, its build, any CI it has.
    #[serde(default)]
    pub status: Vec<String>,
    /// The ways it could join, the best first.
    #[serde(default)]
    pub operations: Vec<Operation>,
    /// Why the CI cannot onboard this origin as it is (a repository outside
    /// its Forgejo, no `[onboarding]`); worked out by the CI, not the model.
    #[serde(default)]
    pub refusal: Option<String>,
    /// The model that wrote it, empty when none did.
    #[serde(default)]
    pub model: String,
    /// Seconds since the UNIX epoch.
    #[serde(default)]
    pub created: u64,
    /// Why no model wrote it: the CI has no `[advice]`, the call failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// What an operation does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Join the CI as it is: build and check jobs.
    #[default]
    Onboard,
    /// Move the repository to the CI's Forgejo first.
    Migrate,
    /// Rebuild when another repository of the CI changes.
    Follow,
    /// Deploy to a host the runners reach.
    Deploy,
    #[serde(other)]
    Other,
}

impl OperationKind {
    pub fn name(self) -> &'static str {
        match self {
            OperationKind::Onboard => "onboard",
            OperationKind::Migrate => "migrate",
            OperationKind::Follow => "follow",
            OperationKind::Deploy => "deploy",
            OperationKind::Other => "other",
        }
    }
}

/// One way a project could join.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    #[serde(default)]
    pub kind: OperationKind,
    /// One line.
    pub title: String,
    /// What it does and why, in Markdown.
    #[serde(default)]
    pub body: String,
    /// The onboarding settings it would file.
    #[serde(default)]
    pub settings: OnboardSettings,
}

/// What an onboarding request asks for: the request's `OnboardSpec` and
/// what its body says the pipeline should do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardSettings {
    /// The repository's name in `ci.toml`, `[a-z0-9][a-z0-9-]*`.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub default_branch: String,
    /// Event patterns: `push main`, `tag v*`, `nightly`, `push main of
    /// <repo>`.
    #[serde(default)]
    pub events: Vec<String>,
    /// Platforms of the CI's `[platforms]` to build for.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Secret name to the event patterns it is released to.
    #[serde(default)]
    pub grants: BTreeMap<String, Vec<String>>,
    /// Host prerequisites the owner sets up by hand, one line each.
    #[serde(default)]
    pub prerequisites: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_operation_kind_reads_as_other() {
        let operation: Operation =
            serde_json::from_str(r#"{"kind":"rewrite","title":"t"}"#).unwrap();
        assert_eq!(operation.kind, OperationKind::Other);
        assert_eq!(operation.settings, OnboardSettings::default());
    }
}
