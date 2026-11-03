//! What the CI advises for a project it does not build yet: a small plan
//! document in Markdown. Its `# ` heading and the text under it say what
//! the project is and where it stands; each `## ` section is a card the
//! owner chooses or drops, and a chosen card becomes a ticket with the
//! section as its proposal.
//!
//! The editor gathers the facts from its local runner (`ProjectsRequest`
//! with `facts`: the project and the skills installed there) and hands them
//! to the CI ([`crate::CiRequest::Advise`]), which asks a language model and
//! answers [`AdviceView`]. The chosen cards are filed with the onboarding in
//! one [`crate::CiRequest::Onboard`]. JSON with defaults throughout: a field
//! added later keeps older peers reading.

use serde::{Deserialize, Serialize};

/// The most facts about one project an editor sends, in bytes.
pub const MAX_FACTS_BYTES: usize = 24 << 10;

/// The most cards one [`AdviceView`] carries.
pub const MAX_CARDS: usize = 8;

/// The heading suffix that leaves a card unchosen until the owner picks it.
const OPTIONAL: &str = "(optional)";

/// The advice for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceView {
    /// As [`crate::normalize_origin`] gives it: `host/owner/name`.
    pub origin: String,
    /// The plan's `# ` heading.
    #[serde(default)]
    pub title: String,
    /// The text under it: what the project is and where it stands, in
    /// Markdown.
    #[serde(default)]
    pub summary: String,
    /// The plan's `## ` sections, in its order.
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

/// One `## ` section of the plan: a piece of work that becomes a ticket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    /// The heading, without `(optional)`.
    pub title: String,
    /// The section's text, the ticket's proposal, in Markdown.
    #[serde(default)]
    pub body: String,
    /// Chosen until the owner says otherwise; a heading that ends in
    /// `(optional)` starts unchosen.
    #[serde(default)]
    pub chosen: bool,
}

/// A plan document taken apart: its `# ` heading, the text under it, and
/// its `## ` sections as cards (at most [`MAX_CARDS`], untitled ones
/// dropped). Headings inside fenced code do not count; deeper headings stay
/// in their section's text.
pub fn parse_plan(markdown: &str) -> (String, String, Vec<Card>) {
    let mut title = String::new();
    let mut summary = String::new();
    let mut cards: Vec<Card> = Vec::new();
    let mut fenced = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        if !fenced {
            if let Some(heading) = trimmed.strip_prefix("## ") {
                let heading = heading.trim().trim_end_matches('#').trim();
                let cut = heading.len().saturating_sub(OPTIONAL.len());
                let optional =
                    heading.is_char_boundary(cut) && heading[cut..].eq_ignore_ascii_case(OPTIONAL);
                let heading = if optional {
                    heading[..cut].trim_end()
                } else {
                    heading
                };
                cards.push(Card {
                    title: heading.to_owned(),
                    body: String::new(),
                    chosen: !optional,
                });
                continue;
            }
            if let Some(heading) = trimmed.strip_prefix("# ")
                && title.is_empty()
                && cards.is_empty()
            {
                title = heading.trim().trim_end_matches('#').trim().to_owned();
                continue;
            }
        }
        let text = match cards.last_mut() {
            Some(card) => &mut card.body,
            None => &mut summary,
        };
        text.push_str(line);
        text.push('\n');
    }
    for card in &mut cards {
        card.body = card.body.trim().to_owned();
    }
    cards.retain(|card| !card.title.is_empty());
    cards.truncate(MAX_CARDS);
    (title, summary.trim().to_owned(), cards)
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
    fn a_plan_becomes_a_summary_and_one_card_per_section() {
        let plan = "\
Preamble the model should not have written.
# Weida's website

A Zola site.
No CI yet.

## Build and check
Run `zola build` on every push.

### Details
```sh
## not a heading
```

## Deploy to sadala (Optional)
rsync the output.
## #
empty heading, dropped
";
        let (title, summary, cards) = parse_plan(plan);
        assert_eq!(title, "Weida's website");
        assert_eq!(
            summary,
            "Preamble the model should not have written.\n\nA Zola site.\nNo CI yet."
        );
        assert_eq!(
            cards,
            [
                Card {
                    title: "Build and check".to_owned(),
                    body: "Run `zola build` on every push.\n\n### Details\n```sh\n## not a heading\n```"
                        .to_owned(),
                    chosen: true,
                },
                Card {
                    title: "Deploy to sadala".to_owned(),
                    body: "rsync the output.".to_owned(),
                    chosen: false,
                },
            ]
        );
    }
}
