//! A re-evaluation of one ticket: the `[advice]` model of `ci.toml`, asked
//! with the CI's Claude sign-in, reads the ticket with its comments and
//! links, the repository's other unfinished tickets and the recent commits
//! of its default branch, and suggests what the owner should do with it now.
//! The caller keeps the answer on the ticket.

use std::path::Path;

use serde::Deserialize;
use zeughaus_link::{
    DocView, MAX_DOC_BYTES, TicketSuggestion, Verdict, text_without_title, ticket_text,
};

use crate::advice;
use crate::claude::Claude;
use crate::config::CiConfig;
use crate::docs;

/// How many commits of the default branch the model reads.
const COMMITS: usize = 30;
/// How many other unfinished tickets it reads, the newest first.
const OTHERS: usize = 40;
/// The longest each part of the message is, in characters.
const OTHER_CHARS: usize = 400;
const BODY_CHARS: usize = 16_000;
const PLAN_CHARS: usize = 8_000;
const COMMENT_CHARS: usize = 2_000;
/// The longest reason kept, in characters.
const REASON_CHARS: usize = 1_000;

/// Asks the model about ticket `number` of `repo`. The answer's `model` and
/// `created` are filled; storing it is the caller's.
pub fn reevaluate(
    state_dir: &Path,
    config: &CiConfig,
    claude: Option<&Claude>,
    repo: &str,
    number: u64,
    now: u64,
) -> Result<TicketSuggestion, String> {
    let (Some(advice), Some(claude)) = (&config.advice, claude) else {
        return Err("the CI has no [advice] and [claude] in ci.toml".to_owned());
    };
    let Some(doc) = docs::load(state_dir, config, repo, number) else {
        return Err(format!("no document #{number} in {repo}"));
    };
    if doc.kind != zeughaus_link::DocKind::Ticket {
        return Err(format!("#{number} is no ticket"));
    }
    if !doc.state.reevaluable() {
        return Err(format!(
            "#{number} is {}; only an open, ready, reviewed or approved ticket is re-evaluated",
            doc.state.name()
        ));
    }
    let mut others: Vec<DocView> = docs::unfinished(state_dir, config, repo)
        .into_iter()
        .filter(|other| other.number != number)
        .collect();
    others.sort_by_key(|other| std::cmp::Reverse(other.number));
    others.truncate(OTHERS);
    let branch = config.repos.get(repo).map(|r| r.default_branch.as_str());
    let commits = recent_commits(state_dir, repo, branch);
    let body = advice::request_body(
        advice,
        &system_prompt(repo, branch),
        &message(&doc, &others, branch, &commits, now),
    );
    let reply = claude
        .access_token(now)
        .and_then(|token| advice::call(advice, &token, &body))?;
    let mut suggestion = parse_answer(&advice::reply_text(&reply)?, number, &others)?;
    suggestion.model = advice.model.clone();
    suggestion.created = now;
    Ok(suggestion)
}

/// The newest commits of `branch` in the CI's mirror of `repo`, one line
/// each, or a line that says why there are none.
fn recent_commits(state_dir: &Path, repo: &str, branch: Option<&str>) -> String {
    let Some(branch) = branch else {
        return format!("(the CI does not build {repo} yet)");
    };
    let mirror = crate::scheduler::mirror_dir(state_dir, repo);
    if !mirror.join("HEAD").exists() {
        return format!("(the CI has not fetched {repo} yet)");
    }
    let count = COMMITS.to_string();
    let reference = format!("refs/heads/{branch}");
    match crate::scheduler::git(
        &mirror,
        &["log", "-n", &count, "--format=%h %cs %s", &reference],
        &[],
    ) {
        Ok(log) if log.trim().is_empty() => "(no commits)".to_owned(),
        Ok(log) => log.trim().to_owned(),
        Err(e) => format!("(git log failed: {e})"),
    }
}

fn system_prompt(repo: &str, branch: Option<&str>) -> String {
    let branch = branch.unwrap_or("the default branch");
    format!(
        "You review one ticket of {repo}, a software project the owner builds with zeughaus-ci, \
         their own CI. A ticket is one piece of work: `open` is a proposal, `ready` waits for a \
         plan session, `review` has a plan that waits for the owner, `approved` is queued for an \
         agent that carries the plan out and pushes. Decide what the owner should do with it \
         now, from its text, its comments, its links, the repository's other unfinished tickets \
         and the recent commits on {branch}:\n\
         - keep: still wanted as it is, but not now;\n\
         - dismiss: already done by a commit, overtaken by another ticket, or no longer worth \
         doing;\n\
         - merge: the same work as another unfinished ticket listed; name it in `into`;\n\
         - plan: wanted and clear enough to plan now;\n\
         - rewrite: still wanted, but its text is out of date or unclear; write the whole new \
         text.\n\
         \n\
         Answer with one JSON object and nothing else: {{\"verdict\": \"keep\" | \"dismiss\" | \
         \"merge\" | \"plan\" | \"rewrite\", \"into\": <ticket number, merge only>, \"reason\": \
         \"<one to three sentences naming the commits, comments or tickets the verdict rests \
         on>\", \"text\": \"<rewrite only: the whole ticket in Markdown, first line `# <title>`, \
         then `Depends on:` / `Related:` lines if it has links, then the text>\"}}. Base the \
         verdict on what is given and say so when it is not enough to decide."
    )
}

/// `today`, `1 day ago` or `<d> days ago` for a time `t`.
fn age(t: u64, now: u64) -> String {
    match now.saturating_sub(t) / 86_400 {
        0 => "today".to_owned(),
        1 => "1 day ago".to_owned(),
        days => format!("{days} days ago"),
    }
}

/// The user message: the ticket, the other unfinished tickets and the
/// commits.
fn message(
    doc: &DocView,
    others: &[DocView],
    branch: Option<&str>,
    commits: &str,
    now: u64,
) -> String {
    let mut text = format!(
        "# Ticket #{}: {}\n\nstate {}, from {}, created {}, last changed {}\n",
        doc.number,
        doc.title,
        doc.state.name(),
        doc.source,
        age(doc.created, now),
        age(doc.changed, now)
    );
    if !doc.links.is_empty() {
        let links: Vec<String> = doc
            .links
            .iter()
            .map(|link| format!("{} {}", link.kind.name(), link.reference(&doc.repo)))
            .collect();
        text.push_str(&format!("links: {}\n", links.join(", ")));
    }
    let body = advice::clip(text_without_title(&doc.body), BODY_CHARS);
    if body.is_empty() {
        text.push_str("\n## Text\n\n(none: it started as a plan)\n");
    } else {
        text.push_str(&format!("\n## Text\n\n{body}\n"));
    }
    if !doc.plan_text.is_empty() {
        text.push_str(&format!(
            "\n## Its plan (revision {})\n\n{}\n",
            doc.revision,
            advice::clip(&doc.plan_text, PLAN_CHARS)
        ));
    }
    if !doc.comments.is_empty() {
        text.push_str("\n## Comments, oldest first\n\n");
        for comment in &doc.comments {
            text.push_str(&format!(
                "- {}, {}: {}\n",
                comment.author,
                age(comment.at, now),
                advice::clip(&comment.text, COMMENT_CHARS)
            ));
        }
    }
    text.push_str("\n## Other unfinished tickets\n\n");
    if others.is_empty() {
        text.push_str("(none)\n");
    }
    for other in others {
        let proposal = text_without_title(&other.body).replace(['\n', '\r'], " ");
        text.push_str(&format!(
            "- #{} {}: {}: {}\n",
            other.number,
            other.state.name(),
            other.title,
            advice::clip(&proposal, OTHER_CHARS)
        ));
    }
    text.push_str(&format!(
        "\n## Recent commits on {}, newest first\n\n{commits}\n",
        branch.unwrap_or("the default branch")
    ));
    text
}

/// The model's answer as a suggestion for ticket `number`, checked against
/// `others`; `model` and `created` are left for the caller.
fn parse_answer(text: &str, number: u64, others: &[DocView]) -> Result<TicketSuggestion, String> {
    #[derive(Deserialize)]
    struct Answer {
        verdict: Verdict,
        #[serde(default)]
        into: Option<u64>,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        text: String,
    }
    let text = text.trim();
    let answer: Answer = match serde_json::from_str(text) {
        Ok(answer) => answer,
        Err(e) => {
            let inner = text
                .find('{')
                .zip(text.rfind('}'))
                .filter(|(start, end)| start < end)
                .map(|(start, end)| &text[start..=end]);
            match inner.map(serde_json::from_str::<Answer>) {
                Some(Ok(answer)) => answer,
                Some(Err(e)) => {
                    return Err(format!("the model's answer is not the asked JSON: {e}"));
                }
                None => return Err(format!("the model's answer is not the asked JSON: {e}")),
            }
        }
    };
    let reason = advice::clip(&answer.reason, REASON_CHARS);
    if reason.is_empty() {
        return Err("the model gave no reason".to_owned());
    }
    let mut suggestion = TicketSuggestion {
        verdict: answer.verdict,
        into: None,
        reason,
        text: String::new(),
        model: String::new(),
        created: 0,
    };
    match answer.verdict {
        Verdict::Merge => match answer.into {
            Some(into) if into != number && others.iter().any(|other| other.number == into) => {
                suggestion.into = Some(into);
            }
            into => {
                return Err(format!(
                    "the model suggested merging into {}, which is no other unfinished ticket",
                    into.map_or_else(|| "nothing".to_owned(), |n| format!("#{n}"))
                ));
            }
        },
        Verdict::Rewrite => {
            ticket_text(&answer.text)
                .map_err(|e| format!("the model's rewrite is no ticket text: {e}"))?;
            if answer.text.len() > MAX_DOC_BYTES {
                return Err(format!(
                    "the model's rewrite is no ticket text: it exceeds {MAX_DOC_BYTES} bytes"
                ));
            }
            suggestion.text = format!("{}\n", answer.text.trim());
        }
        Verdict::Keep | Verdict::Dismiss | Verdict::Plan => {}
    }
    Ok(suggestion)
}

#[cfg(test)]
mod tests {
    use zeughaus_link::{DocKind, DocState, TicketComment};

    use super::*;

    fn ticket(number: u64, title: &str) -> DocView {
        DocView {
            repo: "demo".to_owned(),
            number,
            kind: DocKind::Ticket,
            title: title.to_owned(),
            state: DocState::Open,
            source: "user".to_owned(),
            created: 0,
            changed: 0,
            approved: None,
            released: None,
            links: Vec::new(),
            merged: Vec::new(),
            into: None,
            runs_on: Vec::new(),
            placement: Vec::new(),
            body: format!("# {title}\n\nThe text of {title}.\n"),
            plan_text: String::new(),
            revision: 0,
            planner: None,
            auto_approve: false,
            feedback: String::new(),
            onboard: None,
            note: String::new(),
            waits_for: Vec::new(),
            filed: Vec::new(),
            comments: Vec::new(),
            suggestion: None,
        }
    }

    #[test]
    fn a_stale_tickets_dismissal_is_taken_with_its_reason() {
        let answer = "{\"verdict\":\"dismiss\",\"reason\":\"Commit abc123 did it.\"}";
        let reply = serde_json::json!({
            "content": [{ "type": "text", "text": format!("```json\n{answer}\n```") }],
        })
        .to_string();
        let fenced = advice::reply_text(&reply).unwrap();
        let wrapped = format!("Here is my answer:\n{answer}\nThat is all.");
        for text in [fenced.as_str(), wrapped.as_str()] {
            let suggestion = parse_answer(text, 1, &[]).unwrap();
            assert_eq!(
                (
                    suggestion.verdict,
                    suggestion.reason.as_str(),
                    suggestion.into,
                    suggestion.text.as_str()
                ),
                (Verdict::Dismiss, "Commit abc123 did it.", None, "")
            );
        }
    }

    #[test]
    fn a_merge_needs_another_unfinished_ticket() {
        let others = [ticket(2, "Other")];
        let merge = |into: u64| {
            parse_answer(
                &format!("{{\"verdict\":\"merge\",\"into\":{into},\"reason\":\"Same.\"}}"),
                1,
                &others,
            )
        };
        assert_eq!(
            merge(9).unwrap_err(),
            "the model suggested merging into #9, which is no other unfinished ticket"
        );
        assert!(merge(1).is_err());
        assert_eq!(merge(2).unwrap().into, Some(2));
    }

    #[test]
    fn a_rewrite_must_be_ticket_text() {
        let rewrite = |text: &str| {
            parse_answer(
                &serde_json::json!({ "verdict": "rewrite", "reason": "Old.", "text": text })
                    .to_string(),
                1,
                &[],
            )
        };
        assert!(
            rewrite("no title")
                .unwrap_err()
                .starts_with("the model's rewrite is no ticket text: ")
        );
        assert_eq!(rewrite("# New\n\nBody").unwrap().text, "# New\n\nBody\n");
    }

    #[test]
    fn the_model_is_given_the_comments_and_the_commits() {
        let mut doc = ticket(1, "Stale");
        doc.comments.push(TicketComment {
            author: "hannes@tuco-pc".to_owned(),
            at: 0,
            text: "Is this still needed?".to_owned(),
        });
        let text = message(
            &doc,
            &[ticket(2, "Other")],
            Some("main"),
            "abc1234 2026-11-01 fix: x",
            86_400 * 3,
        );
        assert!(text.contains("- hannes@tuco-pc, 3 days ago: Is this still needed?"));
        assert!(text.contains("abc1234 2026-11-01 fix: x"));
        assert!(text.contains("- #2 open: Other: The text of Other."));
    }
}
