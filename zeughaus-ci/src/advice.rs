//! Advice on a project the CI does not build yet: `[advice]` of `ci.toml`
//! names a model on the Anthropic Messages API, asked with the CI's Claude
//! sign-in (`crate::claude`) the way Claude Code asks. The editor sends the
//! facts its runner gathered; the CI adds what it knows of itself (its
//! Forgejo, platforms, runners and repositories) and asks for a small plan
//! document in Markdown, which [`zeughaus_link::parse_plan`] takes apart
//! into a summary and cards. The answer is kept per origin under
//! `ci/advice/` until the facts or the configuration change.
//!
//! Whether the CI can onboard the origin at all is the CI's own answer
//! ([`AdviceView::refusal`]), never the model's.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeughaus_link::{AdviceView, MAX_FACTS_BYTES, normalize_origin, parse_plan};

use crate::claude::{self, Claude};
use crate::config::{AdviceConfig, CiConfig};

/// The longest the model may write.
const MAX_TOKENS: u32 = 4096;

/// How long one call may take, in seconds.
const TIMEOUT_SECS: u32 = 90;

/// The longest title and text kept, in characters.
const TITLE_CHARS: usize = 100;
const BODY_CHARS: usize = 4000;

/// A kept answer and what it was asked from.
#[derive(Serialize, Deserialize)]
struct Kept {
    /// The SHA-256 of the model and the whole request.
    asked: String,
    advice: AdviceView,
}

/// The advice for `origin` from `facts`: the kept one when the same was
/// asked before and `refresh` is not set, else the model's. Never fails:
/// what went wrong is in [`AdviceView::error`].
pub fn advise(
    state_dir: &Path,
    config: &CiConfig,
    claude: Option<&Claude>,
    origin: &str,
    facts: &str,
    refresh: bool,
) -> AdviceView {
    let origin = normalize_origin(origin);
    let refusal = refusal(config, &origin);
    let mut view = AdviceView {
        origin: origin.clone(),
        refusal,
        ..AdviceView::default()
    };
    let (Some(advice), Some(claude)) = (&config.advice, claude) else {
        view.error = Some("the CI has no [advice] and [claude] in ci.toml".to_owned());
        return view;
    };
    if facts.len() > MAX_FACTS_BYTES {
        view.error = Some(format!("the facts exceed {MAX_FACTS_BYTES} bytes"));
        return view;
    }
    let body = request_body(advice, &system_prompt(config), facts);
    let asked = hex::encode(Sha256::digest(body.as_bytes()));
    let path = kept_path(state_dir, &origin);
    if !refresh
        && let Ok(kept) = crate::files::read_json::<Kept>(&path)
        && kept.asked == asked
    {
        return AdviceView {
            refusal: view.refusal,
            ..kept.advice
        };
    }
    let answer = claude
        .access_token(crate::now_secs())
        .and_then(|token| call(advice, &token, &body))
        .and_then(|reply| parse_reply(&reply));
    match answer {
        Ok(plan) => {
            let (title, summary, cards) = parse_plan(&plan);
            view.title = clip(&title, TITLE_CHARS);
            view.summary = clip(&summary, BODY_CHARS);
            view.cards = cards
                .into_iter()
                .map(|mut card| {
                    card.title = clip(&card.title, TITLE_CHARS);
                    card.body = clip(&card.body, BODY_CHARS);
                    card
                })
                .collect();
            view.model.clone_from(&advice.model);
            view.created = crate::now_secs();
            if let Err(e) = crate::files::write_json(
                &path,
                &Kept {
                    asked,
                    advice: view.clone(),
                },
            ) {
                eprintln!("[ci] advice for {origin} not kept: {e}");
            }
        }
        Err(e) => {
            eprintln!("[ci] advice for {origin}: {e}");
            view.error = Some(e);
        }
    }
    view
}

/// Why the CI cannot onboard `origin` as it is.
fn refusal(config: &CiConfig, origin: &str) -> Option<String> {
    if let Some((name, _)) = config
        .repos
        .iter()
        .find(|(_, repo)| normalize_origin(&repo.url) == origin)
    {
        return Some(format!("{origin} is already {name} in ci.toml"));
    }
    match &config.onboarding {
        Some(onboarding) => crate::onboard::canonical(onboarding, origin).err(),
        None => Some("the CI has no [onboarding] in ci.toml".to_owned()),
    }
}

/// `ci/advice/<sha256 of the origin>.json`.
fn kept_path(state_dir: &Path, origin: &str) -> PathBuf {
    let name = hex::encode(Sha256::digest(origin.as_bytes()));
    crate::ci_dir(state_dir)
        .join("advice")
        .join(format!("{}.json", &name[..32]))
}

/// What the model is told about the CI and the document it writes.
fn system_prompt(config: &CiConfig) -> String {
    let forgejo = config
        .onboarding
        .as_ref()
        .map_or_else(|| "the owner's Forgejo".to_owned(), |o| o.api.clone());
    let mut prompt = format!(
        "You advise the owner of zeughaus-ci, their own CI, on bringing one of their software \
         projects into it.\n\n\
         How the CI works:\n\
         - A repository joins through an onboarding request: its name in the CI and its \
         default branch. On approval the CI adds its bot and a push webhook on {forgejo} and \
         builds the default branch.\n\
         - Onboarding adds repositories of {forgejo} only. A project elsewhere (GitHub) first \
         moves there; its GitHub repository can stay as a push mirror of the Forgejo one.\n\
         - Jobs are scripts in the repository's `.zeughaus-ci/` folder, run on the owner's \
         machines: on a runner's host, in a container image, or for a platform.\n\
         - Events a job runs on: `push <branch>`, `tag v*`, `nightly`, and `push <branch> of \
         <repo>` to follow another repository of the CI (a website that rebuilds when the \
         library it documents changes, a package that follows its upstream).\n\
         - Secrets live in OpenBao and are released to events by grants in the CI's \
         configuration.\n\
         - Each piece of work becomes a ticket: a plan session plans it in the repository, \
         then an agent carries the plan out and pushes.\n\n"
    );
    prompt.push_str("Platforms (platform: where it builds):\n");
    for (platform, target) in &config.platforms {
        let _ = writeln!(prompt, "- {platform}: {target}");
    }
    prompt.push_str("\nRunners (name: os, labels):\n");
    for (name, runner) in &config.runners {
        let os = match runner.os {
            crate::config::RunnerOs::Unix => "unix",
            crate::config::RunnerOs::Windows => "windows",
        };
        let _ = writeln!(prompt, "- {name}: {os}, {}", runner.labels.join(" "));
    }
    prompt.push_str("\nRepositories of the CI (name: origin):\n");
    for (name, repo) in &config.repos {
        let _ = writeln!(prompt, "- {name}: {}", normalize_origin(&repo.url));
    }
    prompt.push_str(
        "\nAnswer with a small plan document in Markdown and nothing else, no code fence \
         around it:\n\n\
         # <what the project is, as a title>\n\n\
         <Two to five sentences: what the project is and where it stands for the CI: where the \
         repository lives and what mirrors it, what it builds with, any CI it has.>\n\n\
         ## <one piece of work>\n\n\
         <What to do and why, concretely enough to plan from: which jobs, events and \
         platforms from the lists above, which files.>\n\n\
         Write 2 to 6 `##` sections, in the order they are to be done: each one waits for the \
         one before it. The owner chooses the sections they want; each chosen one becomes a \
         ticket with the section as its proposal. End a heading with `(optional)` for work \
         the owner may well skip. Name installed skills from the facts that a section's agent \
         should read first as `skill://<name>`. Base everything on the facts and say what you \
         do not know rather than guess. Be brief: headings under 60 characters, sections \
         under 150 words. Use `###` or lists inside a section if needed, never `#` or `##`.\n",
    );
    prompt
}

/// The Messages API request: the system prompt and the facts.
fn request_body(advice: &AdviceConfig, system: &str, facts: &str) -> String {
    serde_json::json!({
        "model": advice.model,
        "max_tokens": MAX_TOKENS,
        // A subscription token is served only to requests whose system
        // prompt opens with Claude Code's own first block.
        "system": [
            { "type": "text", "text": claude::IDENTITY },
            { "type": "text", "text": system },
        ],
        "messages": [{
            "role": "user",
            "content": format!("Facts about the project, gathered by the owner's machine:\n\n{facts}"),
        }],
    })
    .to_string()
}

/// POSTs `body` to the Messages API with `token`, the Claude sign-in's
/// access token, which travels in curl's config on stdin, never in argv.
fn call(advice: &AdviceConfig, token: &str, body: &str) -> Result<String, String> {
    let authorization = format!("Authorization: Bearer {token}");
    let beta = format!("anthropic-beta: {}", claude::MESSAGES_BETA);
    let headers = [
        authorization.as_str(),
        "anthropic-version: 2023-06-01",
        beta.as_str(),
        "Content-Type: application/json",
    ];
    let mut command = Command::new("curl");
    command
        .args(["-sS", "--fail-with-body", "--max-time"])
        .arg(TIMEOUT_SECS.to_string())
        .args(["-K", "-", "-X", "POST"])
        .arg(format!("{}/v1/messages", advice.api));
    crate::forge::run(command, &crate::forge::curl_config(&headers, Some(body)))
        .map_err(|e| format!("the model did not answer: {e}"))
}

/// The plan document of a Messages API reply: its text blocks, without a
/// code fence the model may have put around the whole of it.
fn parse_reply(reply: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Reply {
        content: Vec<Block>,
    }
    #[derive(Deserialize)]
    struct Block {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        text: String,
    }
    let reply: Reply =
        serde_json::from_str(reply).map_err(|e| format!("the model's reply: {e}"))?;
    let text: String = reply
        .content
        .into_iter()
        .filter(|block| block.kind == "text")
        .map(|block| block.text)
        .collect();
    let text = text.trim();
    let text = match text.strip_prefix("```") {
        Some(fenced) => fenced
            .split_once('\n')
            .map_or("", |(_, rest)| rest)
            .trim_end()
            .strip_suffix("```")
            .unwrap_or(fenced),
        None => text,
    };
    if text.trim().is_empty() {
        return Err("the model wrote no plan".to_owned());
    }
    Ok(text.trim().to_owned())
}

/// `text` trimmed and cut after `limit` characters.
fn clip(text: &str, limit: usize) -> String {
    text.trim().chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_is_the_reply_text_without_a_fence_around_it() {
        let reply = r#"{"content":[{"type":"text","text":"```markdown\n# Site\n\nA site.\n\n## Jobs\nBuild it.\n```\n"}]}"#;
        assert_eq!(
            parse_reply(reply).unwrap(),
            "# Site\n\nA site.\n\n## Jobs\nBuild it."
        );
        let plain = r###"{"content":[{"type":"text","text":"# Site\n"},{"type":"text","text":"## Jobs\n"}]}"###;
        assert_eq!(parse_reply(plain).unwrap(), "# Site\n## Jobs");
        assert_eq!(
            parse_reply(r#"{"content":[]}"#).unwrap_err(),
            "the model wrote no plan"
        );
    }
}
