//! Advice on a project the CI does not build yet: `[advice]` of `ci.toml`
//! names a model on the Anthropic Messages API, asked with the CI's Claude
//! sign-in (`crate::claude`) the way Claude Code asks. The editor sends the
//! facts its runner gathered; the CI
//! adds what it knows of itself (its Forgejo, platforms, runners and
//! repositories), asks the model for a forced tool call, keeps only what
//! fits its configuration, and keeps the answer per origin under
//! `ci/advice/` until the facts or the configuration change.
//!
//! Whether the CI can onboard the origin at all is the CI's own answer
//! ([`AdviceView::refusal`]), never the model's.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeughaus_link::{
    AdviceView, Card, CardTicket, MAX_CARD_TICKETS, MAX_CARDS, MAX_FACTS_BYTES, OnboardSettings,
    normalize_origin,
};

use crate::claude::{self, Claude};
use crate::config::{AdviceConfig, CiConfig, valid_env_name, valid_name};
use crate::pipeline::Pattern;

/// The longest the model may write: cards of tickets with proposals.
const MAX_TOKENS: u32 = 8192;

/// How long one call may take, in seconds.
const TIMEOUT_SECS: u32 = 90;

/// The longest title, status line and body kept, in characters.
const TITLE_CHARS: usize = 80;
const LINE_CHARS: usize = 160;
const BODY_CHARS: usize = 2000;

/// The most status lines kept.
const MAX_STATUS: usize = 8;

/// The longest ticket key and skill name kept, in characters.
const KEY_CHARS: usize = 40;

/// The most skills one ticket names.
const MAX_SKILLS: usize = 6;

/// The tool the model must call.
const TOOL: &str = "advise";

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
        Ok(advised) => {
            view.summary = clip(&advised.summary, BODY_CHARS);
            view.status = advised
                .status
                .iter()
                .take(MAX_STATUS)
                .map(|line| clip(line, LINE_CHARS))
                .collect();
            view.onboard = fit_settings(advised.onboard, config, &origin);
            view.cards = fit_cards(advised.cards);
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

/// What the model is told about the CI.
fn system_prompt(config: &CiConfig) -> String {
    let forgejo = config
        .onboarding
        .as_ref()
        .map_or_else(|| "the owner's Forgejo".to_owned(), |o| o.api.clone());
    let mut prompt = format!(
        "You advise the owner of zeughaus-ci, their own CI, on bringing one of their software \
         projects into it. Answer only by calling the tool `{TOOL}`.\n\n\
         How the CI works:\n\
         - A repository joins through an onboarding request: its name in the CI, its default \
         branch, the secrets granted per event, and host prerequisites the owner sets up by hand. \
         On approval the CI adds its bot and a push webhook on {forgejo} and builds the default \
         branch.\n\
         - Onboarding adds repositories of {forgejo} only. A project elsewhere (GitHub) first \
         moves there; its GitHub repository can stay as a push mirror of the Forgejo one.\n\
         - Jobs are scripts in the repository's `.zeughaus-ci/` folder, run on the owner's \
         machines: on a runner's host, in a container image, or for a platform.\n\
         - Events a job runs on: `push <branch>`, `tag v*`, `nightly`, and `push <branch> of \
         <repo>` to follow another repository of the CI (a website that rebuilds when the \
         library it documents changes, a package that follows its upstream).\n\
         - Secrets live in OpenBao and are released to events by grants.\n\n"
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
        "\nStatus: the facts that matter for joining (where the repository lives and what \
         mirrors it, what it builds with, any CI it has).\n\
         Onboard: the onboarding request's settings: the name (lowercase letters, digits and \
         dashes), the default branch, the secrets a job will need by the events they are \
         released to, and host prerequisites the owner sets up by hand.\n\
         Cards: 2 to 6 pieces of work the owner picks from, the most useful first, `chosen` \
         for those you would do in any case. Kinds: `build` (build and check jobs in \
         `.zeughaus-ci/`), `migrate` (move the repository to the Forgejo), `follow` (rebuild \
         when another repository of the CI changes, an event `push main of <repo>`), `deploy` \
         (deploy to a host the runners reach), `other` (fixes the project needs before its \
         pipeline can be green, documentation, tests). A card files 1 to 4 tickets. A ticket \
         is one task a plan session plans and an agent then carries out in the repository: \
         its `proposal` says what to do and why, concretely enough to plan from (which jobs, \
         which events and platforms from the lists above, which files). `key` is a short \
         unique word; `depends_on` names the keys of tickets, of any card, it has to wait for \
         (the jobs before the deploy that uses their output). `skills` names installed skills \
         from the facts its agents should read first. `auto` is true for a ticket whose plan \
         may run without the owner reviewing it (routine work), false where the owner should \
         look at the plan first (anything that deploys, deletes, moves or spends).\n\
         Base everything on the facts and say what you do not know rather than guess. Be \
         brief: titles under 60 characters, status lines under 12 words, card bodies under 60 \
         words, proposals under 150 words.\n",
    );
    prompt
}

/// The Messages API request: the system prompt, the facts, and the tool
/// the model must call.
fn request_body(advice: &AdviceConfig, system: &str, facts: &str) -> String {
    let strings = serde_json::json!({ "type": "array", "items": { "type": "string" } });
    let onboard = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "default_branch": { "type": "string" },
            "grants": {
                "type": "object",
                "description": "secret name to the event patterns it is released to",
                "additionalProperties": strings,
            },
            "prerequisites": strings,
        },
        "required": ["name", "default_branch"],
    });
    let ticket = serde_json::json!({
        "type": "object",
        "properties": {
            "key": { "type": "string" },
            "title": { "type": "string" },
            "proposal": { "type": "string", "description": "Markdown" },
            "skills": strings,
            "depends_on": strings,
            "auto": { "type": "boolean" },
        },
        "required": ["key", "title", "proposal", "depends_on", "auto"],
    });
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string", "description": "what the project is, 2 or 3 sentences" },
            "status": strings,
            "onboard": onboard,
            "cards": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["build", "migrate", "follow", "deploy", "other"] },
                        "title": { "type": "string" },
                        "body": { "type": "string", "description": "Markdown" },
                        "chosen": { "type": "boolean" },
                        "tickets": { "type": "array", "items": ticket },
                    },
                    "required": ["kind", "title", "body", "chosen", "tickets"],
                },
            },
        },
        "required": ["summary", "status", "onboard", "cards"],
    });
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
        "tools": [{
            "name": TOOL,
            "description": "The advice on bringing the project into the CI.",
            "input_schema": schema,
        }],
        "tool_choice": { "type": "tool", "name": TOOL },
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

/// What the model's tool call says.
#[derive(Debug, Default, Deserialize)]
struct Advised {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    status: Vec<String>,
    #[serde(default)]
    onboard: OnboardSettings,
    #[serde(default)]
    cards: Vec<Card>,
}

/// The input of the `advise` tool call in a Messages API reply.
fn parse_reply(reply: &str) -> Result<Advised, String> {
    #[derive(Deserialize)]
    struct Reply {
        content: Vec<serde_json::Value>,
    }
    let reply: Reply =
        serde_json::from_str(reply).map_err(|e| format!("the model's reply: {e}"))?;
    let input = reply
        .content
        .into_iter()
        .find(|block| block.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .and_then(|mut block| block.get_mut("input").map(serde_json::Value::take))
        .ok_or("the model called no tool")?;
    serde_json::from_value(input).map_err(|e| format!("the model's advice: {e}"))
}

/// The onboarding settings cut to what the CI can file: a valid name
/// (else the origin's), a branch, secret names with event patterns that
/// parse and follow one of its repositories, short prerequisites.
fn fit_settings(settings: OnboardSettings, config: &CiConfig, origin: &str) -> OnboardSettings {
    let OnboardSettings {
        name,
        default_branch,
        grants,
        prerequisites,
    } = settings;
    let readable = |event: &String| match Pattern::parse(event) {
        Ok(Pattern::Upstream { repo, .. }) => config.repos.contains_key(&repo),
        Ok(_) => true,
        Err(_) => false,
    };
    let name = if valid_name(&name) {
        name
    } else {
        origin
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_lowercase()
            .replace(['_', '.'], "-")
    };
    let default_branch = match default_branch.trim() {
        "" => "main".to_owned(),
        branch => branch.to_owned(),
    };
    OnboardSettings {
        name,
        default_branch,
        grants: grants
            .into_iter()
            .filter(|(secret, _)| valid_env_name(secret))
            .map(|(secret, events)| (secret, events.into_iter().filter(readable).collect()))
            .collect(),
        prerequisites: prerequisites
            .iter()
            .map(|line| clip(line, LINE_CHARS))
            .filter(|line| !line.is_empty())
            .collect(),
    }
}

/// The cards cut to what can be filed: at most [`MAX_CARDS`] of at most
/// [`MAX_CARD_TICKETS`] tickets with unique keys, dependencies only on
/// tickets that exist and never in a circle (a dependency that would
/// close one is dropped), short texts; a card without tickets goes.
fn fit_cards(cards: Vec<Card>) -> Vec<Card> {
    let mut keys: Vec<String> = Vec::new();
    let mut cards: Vec<Card> = cards
        .into_iter()
        .take(MAX_CARDS)
        .map(|card| {
            let tickets = card
                .tickets
                .into_iter()
                .take(MAX_CARD_TICKETS)
                .filter_map(|ticket| {
                    let key = clip(&ticket.key, KEY_CHARS);
                    if key.is_empty() || keys.contains(&key) || ticket.title.trim().is_empty() {
                        return None;
                    }
                    keys.push(key.clone());
                    Some(CardTicket {
                        key,
                        title: clip(&ticket.title, TITLE_CHARS),
                        proposal: clip(&ticket.proposal, BODY_CHARS),
                        skills: ticket
                            .skills
                            .iter()
                            .map(|skill| clip(skill, KEY_CHARS))
                            .filter(|skill| !skill.is_empty())
                            .take(MAX_SKILLS)
                            .collect(),
                        depends_on: ticket.depends_on,
                        auto: ticket.auto,
                    })
                })
                .collect();
            Card {
                kind: card.kind,
                title: clip(&card.title, TITLE_CHARS),
                body: clip(&card.body, BODY_CHARS),
                chosen: card.chosen,
                tickets,
            }
        })
        .filter(|card| !card.tickets.is_empty())
        .collect();
    // Dependencies in reading order, each kept only when it closes no
    // circle with those kept before it.
    let mut kept: Vec<(String, String)> = Vec::new();
    for card in &mut cards {
        for ticket in &mut card.tickets {
            let wanted = std::mem::take(&mut ticket.depends_on);
            for on in wanted {
                let on = on.trim().to_owned();
                if on == ticket.key
                    || !keys.contains(&on)
                    || ticket.depends_on.contains(&on)
                    || reaches(&kept, &on, &ticket.key)
                {
                    continue;
                }
                kept.push((ticket.key.clone(), on.clone()));
                ticket.depends_on.push(on);
            }
        }
    }
    cards
}

/// Whether `from` depends on `to` through `edges` (ticket, what it waits
/// for).
fn reaches(edges: &[(String, String)], from: &str, to: &str) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    let mut open = vec![from];
    while let Some(at) = open.pop() {
        if at == to {
            return true;
        }
        if seen.contains(&at) {
            continue;
        }
        seen.push(at);
        open.extend(
            edges
                .iter()
                .filter(|(ticket, _)| ticket == at)
                .map(|(_, on)| on.as_str()),
        );
    }
    false
}

/// `text` trimmed and cut after `limit` characters.
fn clip(text: &str, limit: usize) -> String {
    text.trim().chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::CardKind;

    fn config() -> CiConfig {
        CiConfig::parse(
            "default_runner = \"tuco-pc\"\n\
             [runners.tuco-pc]\nfingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n\
             [platforms]\nlinux-amd64 = \"tuco-pc\"\n\
             [repos.weida]\nurl = \"https://git.doodleshnookie.net/tuco86/weida.git\"\n",
        )
        .expect("a valid ci.toml")
    }

    #[test]
    fn the_settings_keep_only_what_the_ci_can_file() {
        let settings = OnboardSettings {
            name: "Weida_Site".to_owned(),
            default_branch: " ".to_owned(),
            grants: [
                (
                    "DEPLOY_KEY".to_owned(),
                    vec![
                        "push main".to_owned(),
                        "push main of nowhere".to_owned(),
                        "always".to_owned(),
                    ],
                ),
                ("deploy key".to_owned(), vec!["push main".to_owned()]),
            ]
            .into(),
            prerequisites: vec!["  a served directory ".to_owned(), " ".to_owned()],
        };
        assert_eq!(
            fit_settings(
                settings,
                &config(),
                "git.doodleshnookie.net/tuco86/weida_site"
            ),
            OnboardSettings {
                name: "weida-site".to_owned(),
                default_branch: "main".to_owned(),
                grants: [("DEPLOY_KEY".to_owned(), vec!["push main".to_owned()])].into(),
                prerequisites: vec!["a served directory".to_owned()],
            }
        );
    }

    fn ticket(key: &str, depends_on: &[&str]) -> CardTicket {
        CardTicket {
            key: key.to_owned(),
            title: format!("Do {key}"),
            proposal: "p".to_owned(),
            skills: Vec::new(),
            depends_on: depends_on.iter().map(|on| (*on).to_owned()).collect(),
            auto: false,
        }
    }

    #[test]
    fn cards_keep_unique_keys_and_no_circles_or_strangers_among_dependencies() {
        let card = |tickets| Card {
            kind: CardKind::Build,
            title: "c".to_owned(),
            body: String::new(),
            chosen: true,
            tickets,
        };
        let fitted = fit_cards(vec![
            card(vec![
                ticket("jobs", &["deploy", "nobody", "jobs"]),
                ticket("jobs", &[]),
            ]),
            card(vec![ticket("deploy", &["jobs", "jobs"])]),
            card(Vec::new()),
        ]);
        let edges: Vec<(&str, Vec<&str>)> = fitted
            .iter()
            .flat_map(|card| &card.tickets)
            .map(|t| {
                (
                    t.key.as_str(),
                    t.depends_on.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        // The first dependency read stands; the one that would close the
        // circle goes, as do the unknown one, the self link, the duplicate
        // key and the empty card.
        assert_eq!(edges, [("jobs", vec!["deploy"]), ("deploy", vec![])]);
    }

    #[test]
    fn the_advice_is_the_input_of_the_tool_call() {
        let reply = r#"{"content":[{"type":"text","text":"Here."},
            {"type":"tool_use","name":"advise","input":{"summary":"A site.",
            "status":["on GitHub"],"onboard":{"name":"site","default_branch":"main"},
            "cards":[{"kind":"migrate","title":"Move","body":"b","chosen":true,
            "tickets":[{"key":"move","title":"Move it","proposal":"p","depends_on":[],"auto":false}]}]}}]}"#;
        let advised = parse_reply(reply).unwrap();
        assert_eq!(advised.summary, "A site.");
        assert_eq!(advised.cards[0].kind, CardKind::Migrate);
        assert_eq!(advised.cards[0].tickets[0].key, "move");
        assert_eq!(
            parse_reply(r#"{"content":[{"type":"text","text":"no"}]}"#).unwrap_err(),
            "the model called no tool"
        );
    }
}
