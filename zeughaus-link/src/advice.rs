//! What the CI advises for a project it does not build yet: what the
//! project is, where it stands, how it would join, and cards of work to
//! choose from, each the tickets it files.
//!
//! The editor gathers the facts from its local runner (`ProjectsRequest`
//! with `facts`: the project and the skills installed there) and hands them
//! to the CI ([`crate::CiRequest::Advise`]), which asks a language model and
//! answers [`AdviceView`]. The cards the owner keeps are filed with the
//! onboarding in one [`crate::CiRequest::Onboard`]. JSON with defaults
//! throughout: a field added later keeps older peers reading.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The most facts about one project an editor sends, in bytes.
pub const MAX_FACTS_BYTES: usize = 24 << 10;

/// The most cards one [`AdviceView`] carries.
pub const MAX_CARDS: usize = 6;

/// The most tickets one card files.
pub const MAX_CARD_TICKETS: usize = 4;

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
    /// How it joins: the onboarding request's settings.
    #[serde(default)]
    pub onboard: OnboardSettings,
    /// Work to choose from, the most useful first.
    #[serde(default)]
    pub cards: Vec<Card>,
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

/// What a card is about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    /// Build and check jobs.
    #[default]
    Build,
    /// Move the repository to the CI's Forgejo.
    Migrate,
    /// Rebuild when another repository of the CI changes.
    Follow,
    /// Deploy to a host the runners reach.
    Deploy,
    #[serde(other)]
    Other,
}

impl CardKind {
    pub fn name(self) -> &'static str {
        match self {
            CardKind::Build => "build",
            CardKind::Migrate => "migrate",
            CardKind::Follow => "follow",
            CardKind::Deploy => "deploy",
            CardKind::Other => "other",
        }
    }
}

/// One piece of work the project could get with its onboarding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    #[serde(default)]
    pub kind: CardKind,
    /// One line.
    pub title: String,
    /// What it brings and why, in Markdown.
    #[serde(default)]
    pub body: String,
    /// Whether the card is chosen until the owner says otherwise.
    #[serde(default)]
    pub chosen: bool,
    #[serde(default)]
    pub tickets: Vec<CardTicket>,
}

/// One ticket a card files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardTicket {
    /// Unique among the advice's tickets; what `depends_on` names.
    pub key: String,
    pub title: String,
    /// The proposal: what its plan session is to plan, in Markdown.
    #[serde(default)]
    pub proposal: String,
    /// Installed skills its sessions should read first.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Keys of the tickets it waits for, of any card.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Its plan is approved as soon as it is written (it runs through to
    /// its master); else the plan waits for the owner's review.
    #[serde(default)]
    pub auto: bool,
}

/// What an onboarding request asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardSettings {
    /// The repository's name in `ci.toml`, `[a-z0-9][a-z0-9-]*`.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub default_branch: String,
    /// Secret name to the event patterns it is released to.
    #[serde(default)]
    pub grants: BTreeMap<String, Vec<String>>,
    /// Host prerequisites the owner sets up by hand, one line each.
    #[serde(default)]
    pub prerequisites: Vec<String>,
}

/// One ticket an onboarding files with the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTicket {
    pub title: String,
    /// The proposal.
    pub body: String,
    /// Indexes of the tickets of the same request it depends on.
    #[serde(default)]
    pub depends_on: Vec<usize>,
    #[serde(default)]
    pub auto_approve: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_card_kind_reads_as_other() {
        let card: Card = serde_json::from_str(r#"{"kind":"rewrite","title":"t"}"#).unwrap();
        assert_eq!(card.kind, CardKind::Other);
        assert!(card.tickets.is_empty());
    }
}
