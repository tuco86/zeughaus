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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeughaus_link::{
    AdviceView, MAX_FACTS_BYTES, MAX_OPERATIONS, OnboardSettings, Operation, normalize_origin,
};

use crate::claude::{self, Claude};
use crate::config::{AdviceConfig, CiConfig, valid_env_name, valid_name};
use crate::pipeline::Pattern;

/// The longest the model may write.
const MAX_TOKENS: u32 = 2048;

/// How long one call may take, in seconds.
const TIMEOUT_SECS: u32 = 90;

/// The longest title, status line and body kept, in characters.
const TITLE_CHARS: usize = 80;
const LINE_CHARS: usize = 160;
const BODY_CHARS: usize = 2000;

/// The most status lines kept.
const MAX_STATUS: usize = 8;

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
            view.operations = advised
                .operations
                .into_iter()
                .take(MAX_OPERATIONS)
                .map(|operation| fit(operation, config, &origin))
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
         Operations: 1 to 4, the most useful first. Kinds: `onboard` (join as it is, with build \
         and check jobs), `migrate` (move to the Forgejo first, then join), `follow` (rebuild \
         when another repository of the CI changes, named in an event `push main of <repo>`), \
         `deploy` (deploy to a host the runners reach, with the secrets and host prerequisites \
         that needs), `other`. Each operation's settings are what its onboarding request \
         would file: platforms only from the list above, events only in the forms above, the \
         name lowercase letters, digits and dashes. The body says what the operation does and \
         which jobs it would add.\n\
         Base everything on the facts and say what you do not know rather than guess. Be \
         brief: titles under 60 characters, status lines under 12 words, bodies under 120 \
         words.\n",
    );
    prompt
}

/// The Messages API request: the system prompt, the facts, and the tool
/// the model must call.
fn request_body(advice: &AdviceConfig, system: &str, facts: &str) -> String {
    let strings = serde_json::json!({ "type": "array", "items": { "type": "string" } });
    let settings = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "default_branch": { "type": "string" },
            "events": strings,
            "platforms": strings,
            "grants": {
                "type": "object",
                "description": "secret name to the event patterns it is released to",
                "additionalProperties": strings,
            },
            "prerequisites": strings,
        },
        "required": ["name", "default_branch", "events", "platforms"],
    });
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string", "description": "what the project is, 2 or 3 sentences" },
            "status": strings,
            "operations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["onboard", "migrate", "follow", "deploy", "other"] },
                        "title": { "type": "string" },
                        "body": { "type": "string", "description": "Markdown" },
                        "settings": settings,
                    },
                    "required": ["kind", "title", "body", "settings"],
                },
            },
        },
        "required": ["summary", "status", "operations"],
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
    operations: Vec<Operation>,
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

/// `operation` cut to what the CI can file: its platforms, readable event
/// patterns, a valid name and secret names, short texts.
fn fit(operation: Operation, config: &CiConfig, origin: &str) -> Operation {
    let OnboardSettings {
        name,
        default_branch,
        events,
        platforms,
        grants,
        prerequisites,
    } = operation.settings;
    // A followed repository must be one of the CI's.
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
    let grants: BTreeMap<String, Vec<String>> = grants
        .into_iter()
        .filter(|(secret, _)| valid_env_name(secret))
        .map(|(secret, events)| (secret, events.into_iter().filter(readable).collect()))
        .collect();
    Operation {
        kind: operation.kind,
        title: clip(&operation.title, TITLE_CHARS),
        body: clip(&operation.body, BODY_CHARS),
        settings: OnboardSettings {
            name,
            default_branch,
            events: events.into_iter().filter(readable).collect(),
            platforms: platforms
                .into_iter()
                .filter(|p| config.platforms.contains_key(p))
                .collect(),
            grants,
            prerequisites: prerequisites
                .iter()
                .map(|line| clip(line, LINE_CHARS))
                .filter(|line| !line.is_empty())
                .collect(),
        },
    }
}

/// `text` trimmed and cut after `limit` characters.
fn clip(text: &str, limit: usize) -> String {
    text.trim().chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::OperationKind;

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
    fn an_operation_keeps_only_what_the_ci_can_file() {
        let operation = Operation {
            kind: OperationKind::Follow,
            title: "Follow weida".to_owned(),
            body: "Rebuild.".to_owned(),
            settings: OnboardSettings {
                name: "Weida_Site".to_owned(),
                default_branch: " ".to_owned(),
                events: vec![
                    "push main of weida".to_owned(),
                    "on every commit".to_owned(),
                ],
                platforms: vec!["linux-amd64".to_owned(), "plan9".to_owned()],
                grants: [
                    (
                        "DEPLOY_KEY".to_owned(),
                        vec!["push main".to_owned(), "always".to_owned()],
                    ),
                    ("deploy key".to_owned(), vec!["push main".to_owned()]),
                ]
                .into(),
                prerequisites: vec!["  a served directory ".to_owned(), " ".to_owned()],
            },
        };
        let fitted = fit(
            operation,
            &config(),
            "git.doodleshnookie.net/tuco86/weida_site",
        );
        assert_eq!(
            fitted.settings,
            OnboardSettings {
                name: "weida-site".to_owned(),
                default_branch: "main".to_owned(),
                events: vec!["push main of weida".to_owned()],
                platforms: vec!["linux-amd64".to_owned()],
                grants: [("DEPLOY_KEY".to_owned(), vec!["push main".to_owned()])].into(),
                prerequisites: vec!["a served directory".to_owned()],
            }
        );
    }

    #[test]
    fn the_advice_is_the_input_of_the_tool_call() {
        let reply = r#"{"content":[{"type":"text","text":"Here."},
            {"type":"tool_use","name":"advise","input":{"summary":"A site.",
            "status":["on GitHub"],"operations":[{"kind":"migrate","title":"Move",
            "body":"b","settings":{"name":"site","default_branch":"main","events":[],
            "platforms":[]}}]}}]}"#;
        let advised = parse_reply(reply).unwrap();
        assert_eq!(advised.summary, "A site.");
        assert_eq!(advised.operations[0].kind, OperationKind::Migrate);
        assert_eq!(
            parse_reply(r#"{"content":[{"type":"text","text":"no"}]}"#).unwrap_err(),
            "the model called no tool"
        );
    }
}
