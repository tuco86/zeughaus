//! Webhook intake: `POST /hook/<repo>` from GitHub or Forgejo becomes an inbox
//! event. The scheduler, not this process, decides what runs.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::config::{CiConfig, RepoConfig, read_secret};
use super::event::{CiEvent, EventKind, write_inbox};
use super::{ci_dir, now_secs};
use crate::files::write_atomic;

/// Largest accepted request body.
const MAX_BODY: usize = 5 * 1024 * 1024;
/// Delivery ids remembered for duplicate detection.
const KEEP_DELIVERIES: usize = 1000;

type Response = (u16, &'static str, Option<CiEvent>);

/// Recently seen delivery ids, oldest first.
#[derive(Debug, Default)]
pub struct Deliveries {
    ids: Vec<String>,
    dirty: bool,
}

impl Deliveries {
    fn path(state_dir: &Path) -> PathBuf {
        ci_dir(state_dir).join("deliveries")
    }

    fn load(state_dir: &Path) -> Deliveries {
        let ids = std::fs::read_to_string(Self::path(state_dir))
            .map(|text| {
                text.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Deliveries { ids, dirty: false }
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|seen| seen == id)
    }

    fn record(&mut self, id: &str) {
        self.ids.push(id.to_owned());
        if self.ids.len() > KEEP_DELIVERIES {
            let excess = self.ids.len() - KEEP_DELIVERIES;
            self.ids.drain(..excess);
        }
        self.dirty = true;
    }

    fn save(&mut self, state_dir: &Path) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        let mut text = self.ids.join("\n");
        text.push('\n');
        write_atomic(&Self::path(state_dir), text.as_bytes())?;
        self.dirty = false;
        Ok(())
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Constant-time check of `sig_hex` against HMAC-SHA256 of `body`.
fn signature_ok(secret: &[u8], body: &[u8], sig_hex: &str) -> bool {
    let Ok(expected) = hex::decode(sig_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Decides the answer to one request. Returns the event to enqueue, if any.
/// Delivery ids are recorded in `deliveries`; the caller persists them.
#[allow(clippy::too_many_arguments)]
fn handle(
    repos: &BTreeMap<String, RepoConfig>,
    secrets: &BTreeMap<String, Vec<u8>>,
    deliveries: &mut Deliveries,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    now: u64,
) -> Response {
    let path = url.split('?').next().unwrap_or("");
    let Some(name) = path.strip_prefix("/hook/") else {
        return (404, "not found", None);
    };
    if !method.eq_ignore_ascii_case("POST") {
        return (404, "not found", None);
    }
    let Some(repo) = repos.get(name) else {
        return (404, "not found", None);
    };
    if body.len() > MAX_BODY {
        return (413, "payload too large", None);
    }

    let (event_name, sig_hex, delivery) = if let Some(e) = header(headers, "x-github-event") {
        let sig = header(headers, "x-hub-signature-256").and_then(|s| s.strip_prefix("sha256="));
        (e, sig, header(headers, "x-github-delivery"))
    } else if let Some(e) = header(headers, "x-forgejo-event") {
        (
            e,
            header(headers, "x-forgejo-signature"),
            header(headers, "x-forgejo-delivery"),
        )
    } else {
        return (400, "unknown forge", None);
    };

    let (Some(secret), Some(sig)) = (secrets.get(name), sig_hex) else {
        return (401, "unauthorized", None);
    };
    if !signature_ok(secret, body, sig) {
        return (401, "unauthorized", None);
    }

    if event_name == "ping" {
        return (200, "pong", None);
    }
    if let Some(id) = delivery
        && deliveries.contains(id)
    {
        return (200, "duplicate", None);
    }
    let remember = |deliveries: &mut Deliveries| {
        if let Some(id) = delivery {
            deliveries.record(id);
        }
    };
    if event_name != "push" {
        remember(deliveries);
        return (202, "ignored", None);
    }

    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (400, "bad payload", None);
    };
    let full_name = json
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !full_name.eq_ignore_ascii_case(&repo.slug) {
        return (403, "forbidden", None);
    }

    let after = json.get("after").and_then(|v| v.as_str()).unwrap_or("");
    let deleted = json.get("deleted").and_then(|v| v.as_bool()) == Some(true);
    if deleted || after.is_empty() || after.bytes().all(|b| b == b'0') {
        remember(deliveries);
        return (202, "ignored", None);
    }
    let message = json
        .pointer("/head_commit/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if message.contains("[skip ci]") || message.contains("[ci skip]") {
        remember(deliveries);
        return (202, "skipped", None);
    }

    let git_ref = json.get("ref").and_then(|v| v.as_str()).unwrap_or("");
    let (kind, short) = if let Some(b) = git_ref.strip_prefix("refs/heads/") {
        (EventKind::Push, b)
    } else if let Some(t) = git_ref.strip_prefix("refs/tags/") {
        (EventKind::Tag, t)
    } else {
        remember(deliveries);
        return (202, "ignored", None);
    };
    if short.is_empty() {
        remember(deliveries);
        return (202, "ignored", None);
    }

    let actor = json
        .pointer("/sender/login")
        .and_then(|v| v.as_str())
        .or_else(|| json.pointer("/pusher/name").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    remember(deliveries);
    (
        202,
        "queued",
        Some(CiEvent {
            repo: name.to_owned(),
            kind,
            git_ref: short.to_owned(),
            sha: Some(after.to_owned()),
            cron: None,
            actor: actor.to_owned(),
            delivery: delivery.map(str::to_owned),
            received: now,
        }),
    )
}

fn listen_arg(args: &[String]) -> Option<&str> {
    let at = args.iter().position(|a| a == "--listen")?;
    args.get(at + 1).map(String::as_str)
}

pub fn run(state_dir: &Path, args: &[String]) -> ExitCode {
    let Some(addr) = listen_arg(args) else {
        eprintln!("[ci-hook] usage: ci hook --listen <addr>");
        return ExitCode::FAILURE;
    };
    let config = match CiConfig::load(state_dir) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!("[ci-hook] no ci.toml");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("[ci-hook] {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut secrets = BTreeMap::new();
    for (name, repo) in &config.repos {
        let Some(secret_name) = &repo.webhook_secret else {
            eprintln!("[ci-hook] {name}: no webhook_secret; its hook answers 401");
            continue;
        };
        match read_secret(state_dir, secret_name) {
            Ok(value) => {
                secrets.insert(name.clone(), value.into_bytes());
            }
            Err(e) => eprintln!("[ci-hook] {name}: {e}; its hook answers 401"),
        }
    }

    let server = match tiny_http::Server::http(addr) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("[ci-hook] cannot listen on {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("[ci-hook] listening on {addr}");

    let mut deliveries = Deliveries::load(state_dir);
    for mut request in server.incoming_requests() {
        let method = request.method().to_string();
        let url = request.url().to_owned();
        let headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|h| {
                (
                    h.field.as_str().as_str().to_ascii_lowercase(),
                    h.value.as_str().to_owned(),
                )
            })
            .collect();

        let mut body = Vec::new();
        let too_large = request.body_length().is_some_and(|n| n > MAX_BODY) || {
            let read = request
                .as_reader()
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut body);
            read.is_err() || body.len() > MAX_BODY
        };

        let (code, text, event) = if too_large {
            (413, "payload too large", None)
        } else {
            handle(
                &config.repos,
                &secrets,
                &mut deliveries,
                &method,
                &url,
                &headers,
                &body,
                now_secs(),
            )
        };

        let (code, text) = match &event {
            Some(event) => match write_inbox(state_dir, event) {
                Ok(_) => (code, text),
                Err(e) => {
                    eprintln!("[ci-hook] inbox: {e}");
                    // The forge redelivers on a 5xx; the id recorded for
                    // this attempt must not turn that into a "duplicate".
                    deliveries = Deliveries::load(state_dir);
                    (500, "internal error")
                }
            },
            None => (code, text),
        };
        if let Err(e) = deliveries.save(state_dir) {
            eprintln!("[ci-hook] deliveries: {e}");
        }

        let repo = url
            .split('?')
            .next()
            .and_then(|p| p.strip_prefix("/hook/"))
            .unwrap_or("-");
        let event_name = headers
            .iter()
            .find(|(n, _)| n == "x-github-event" || n == "x-forgejo-event")
            .map_or("-", |(_, v)| v.as_str());
        eprintln!("[ci-hook] {repo} {event_name} {code} {text}");

        let response = tiny_http::Response::from_string(text).with_status_code(code);
        if let Err(e) = request.respond(response) {
            eprintln!("[ci-hook] respond: {e}");
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret";

    fn repos() -> BTreeMap<String, RepoConfig> {
        let toml = r#"
[repos.griasdi]
url = "https://github.com/Griasdi/Griasdi.git"
forge = "github"
slug = "Griasdi/Griasdi"
api = "https://api.github.com"
webhook_secret = "HOOK_SECRET"
"#;
        CiConfig::parse(toml).expect("config").repos
    }

    fn secrets() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([("griasdi".to_owned(), SECRET.to_vec())])
    }

    fn sign(body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).expect("key");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn push_body(git_ref: &str, extra: &str) -> Vec<u8> {
        format!(
            r#"{{"ref":"{git_ref}","after":"{}","repository":{{"full_name":"griasdi/griasdi"}},
"sender":{{"login":"alice"}}{extra}}}"#,
            "a".repeat(40)
        )
        .into_bytes()
    }

    fn github(event: &str, delivery: &str, sig: &str) -> Vec<(String, String)> {
        vec![
            ("x-github-event".into(), event.into()),
            ("x-github-delivery".into(), delivery.into()),
            ("x-hub-signature-256".into(), format!("sha256={sig}")),
        ]
    }

    fn post(d: &mut Deliveries, headers: &[(String, String)], body: &[u8], url: &str) -> Response {
        handle(&repos(), &secrets(), d, "POST", url, headers, body, 42)
    }

    #[test]
    fn valid_github_push_is_queued() {
        let body = push_body("refs/heads/main", "");
        let mut d = Deliveries::default();
        let (code, text, event) = post(
            &mut d,
            &github("push", "d1", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        assert_eq!((code, text), (202, "queued"));
        let event = event.expect("event");
        assert_eq!(event.repo, "griasdi");
        assert_eq!(event.kind, EventKind::Push);
        assert_eq!(event.git_ref, "main");
        assert_eq!(event.sha.as_deref(), Some("a".repeat(40).as_str()));
        assert_eq!(event.actor, "alice");
        assert_eq!(event.delivery.as_deref(), Some("d1"));
        assert_eq!(event.received, 42);
    }

    #[test]
    fn invalid_signature_is_401() {
        let body = push_body("refs/heads/main", "");
        let mut d = Deliveries::default();
        let bad = sign(b"other");
        assert_eq!(
            post(&mut d, &github("push", "d1", &bad), &body, "/hook/griasdi").0,
            401
        );
        let missing = vec![("x-github-event".to_owned(), "push".to_owned())];
        assert_eq!(post(&mut d, &missing, &body, "/hook/griasdi").0, 401);
    }

    #[test]
    fn valid_forgejo_signature_is_accepted() {
        let body = push_body("refs/heads/main", "");
        let headers = vec![
            ("x-forgejo-event".to_owned(), "push".to_owned()),
            ("x-forgejo-delivery".to_owned(), "f1".to_owned()),
            ("x-forgejo-signature".to_owned(), sign(&body)),
        ];
        let mut d = Deliveries::default();
        let (code, text, event) = post(&mut d, &headers, &body, "/hook/griasdi");
        assert_eq!((code, text), (202, "queued"));
        assert!(event.is_some());
    }

    #[test]
    fn tag_ref_maps_to_tag() {
        let body = push_body("refs/tags/v1.2.3", "");
        let mut d = Deliveries::default();
        let (_, _, event) = post(
            &mut d,
            &github("push", "t1", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        let event = event.expect("event");
        assert_eq!(event.kind, EventKind::Tag);
        assert_eq!(event.git_ref, "v1.2.3");
    }

    #[test]
    fn deleted_branch_is_ignored() {
        let body = format!(
            r#"{{"ref":"refs/heads/x","after":"{}","deleted":true,"repository":{{"full_name":"Griasdi/Griasdi"}}}}"#,
            "0".repeat(40)
        )
        .into_bytes();
        let mut d = Deliveries::default();
        let (code, text, event) = post(
            &mut d,
            &github("push", "x1", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        assert_eq!((code, text), (202, "ignored"));
        assert!(event.is_none());
    }

    #[test]
    fn skip_ci_is_skipped() {
        let body = push_body(
            "refs/heads/main",
            r#","head_commit":{"message":"wip [skip ci]"}"#,
        );
        let mut d = Deliveries::default();
        let (code, text, event) = post(
            &mut d,
            &github("push", "s1", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        assert_eq!((code, text), (202, "skipped"));
        assert!(event.is_none());
    }

    #[test]
    fn duplicate_delivery_is_reported() {
        let body = push_body("refs/heads/main", "");
        let headers = github("push", "dup", &sign(&body));
        let mut d = Deliveries::default();
        assert_eq!(post(&mut d, &headers, &body, "/hook/griasdi").1, "queued");
        let (code, text, event) = post(&mut d, &headers, &body, "/hook/griasdi");
        assert_eq!((code, text), (200, "duplicate"));
        assert!(event.is_none());
    }

    #[test]
    fn ping_answers_pong_and_non_push_is_ignored() {
        let body = b"{}".to_vec();
        let mut d = Deliveries::default();
        let (code, text, _) = post(
            &mut d,
            &github("ping", "p1", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        assert_eq!((code, text), (200, "pong"));
        let (code, text, _) = post(
            &mut d,
            &github("issues", "p2", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        assert_eq!((code, text), (202, "ignored"));
    }

    #[test]
    fn slug_mismatch_is_403() {
        let body =
            br#"{"ref":"refs/heads/main","after":"abc","repository":{"full_name":"evil/repo"}}"#
                .to_vec();
        let mut d = Deliveries::default();
        let (code, _, event) = post(
            &mut d,
            &github("push", "m1", &sign(&body)),
            &body,
            "/hook/griasdi",
        );
        assert_eq!(code, 403);
        assert!(event.is_none());
    }

    #[test]
    fn unknown_path_repo_and_method_are_404() {
        let body = push_body("refs/heads/main", "");
        let headers = github("push", "n1", &sign(&body));
        let mut d = Deliveries::default();
        assert_eq!(post(&mut d, &headers, &body, "/hook/nope").0, 404);
        assert_eq!(post(&mut d, &headers, &body, "/other").0, 404);
        let get = handle(
            &repos(),
            &secrets(),
            &mut d,
            "GET",
            "/hook/griasdi",
            &headers,
            &body,
            1,
        );
        assert_eq!(get.0, 404);
    }

    #[test]
    fn deliveries_keep_newest() {
        let mut d = Deliveries::default();
        for i in 0..(KEEP_DELIVERIES + 5) {
            d.record(&i.to_string());
        }
        assert_eq!(d.ids.len(), KEEP_DELIVERIES);
        assert!(!d.contains("4"));
        assert!(d.contains("5"));
    }
}
