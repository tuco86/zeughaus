//! Onboarding requests: a project asks to join the CI, the owner approves
//! the request, and the CI sets the repository up itself. It adds its bot as
//! a collaborator, stores a webhook secret in OpenBao, creates the hook,
//! registers the repository as `ci/onboarded/<name>.toml` with a hot reload
//! and queues the first push pipeline.
//!
//! Every step can run again: the collaborator PUT is idempotent, a stored
//! secret is reused, an existing hook is updated, and a registered
//! repository is not written twice. A failed attempt returns the request to
//! `open` with its note; the owner fixes the cause and approves it again.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};

use zeughaus_link::ci::{DocView, OnboardSpec, normalize_origin};

use crate::config::{
    CiConfig, Forge, OnboardingConfig, RepoConfig, Shared, check_grants, onboarded_dir, valid_name,
};
use crate::docs;
use crate::event::{self, CiEvent, EventKind};
use crate::forge;
use crate::secrets::Secrets;

/// The most secrets one request may grant.
const MAX_GRANTS: usize = 32;
/// The most host prerequisites one request may list.
const MAX_PREREQUISITES: usize = 32;

/// `origin` as a repository of `[onboarding] api`'s Forgejo: its
/// `owner/name` slug and its canonical clone url.
pub fn canonical(onb: &OnboardingConfig, origin: &str) -> Result<(String, String), String> {
    let host = normalize_origin(&onb.api);
    let wanted = normalize_origin(origin);
    let Some(slug) = wanted.strip_prefix(&format!("{host}/")) else {
        return Err(format!(
            "{origin} is not on {}; onboarding adds repositories of that Forgejo only",
            onb.api
        ));
    };
    match slug.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((slug.to_owned(), format!("{}/{slug}.git", onb.api)))
        }
        _ => Err(format!(
            "{origin} names no owner/name repository on {}",
            onb.api
        )),
    }
}

/// The OpenBao field of repository `repo`'s webhook secret:
/// `weida-site` is `WEIDA_SITE_WEBHOOK_SECRET`.
pub fn webhook_secret_name(repo: &str) -> String {
    format!("{}_WEBHOOK_SECRET", repo.to_uppercase().replace('-', "_"))
}

/// The `[repos.<name>]` table of an onboarded repository.
pub fn repo_config(
    onb: &OnboardingConfig,
    name: &str,
    spec: &OnboardSpec,
    slug: &str,
) -> RepoConfig {
    RepoConfig {
        url: spec.url.clone(),
        forge: Forge::Forgejo,
        slug: slug.to_owned(),
        api: onb.api.clone(),
        default_branch: spec.default_branch.clone(),
        fetch_token: Some(onb.bot_token.clone()),
        fetch_user: onb.bot.clone(),
        status_token: Some(onb.bot_token.clone()),
        webhook_secret: Some(webhook_secret_name(name)),
        grants: spec.grants.clone(),
        agent_sessions: false,
    }
}

/// Checks a new request for repository `repo` and answers its spec with
/// the url canonical.
pub fn check_request(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    spec: &OnboardSpec,
) -> Result<OnboardSpec, String> {
    let Some(onb) = &config.onboarding else {
        return Err("the CI takes no onboarding requests: ci.toml has no [onboarding]".to_owned());
    };
    if !valid_name(repo) {
        return Err("a repository name matches [a-z0-9][a-z0-9-]*".to_owned());
    }
    if config.repos.contains_key(repo) {
        return Err(format!("{repo} is a repository of ci.toml already"));
    }
    let pending = docs::pending(state_dir, config);
    if let Some(doc) = pending.iter().find(|doc| doc.repo == repo) {
        return Err(format!(
            "{repo} has onboarding request #{} already ({})",
            doc.number,
            doc.state.name()
        ));
    }
    let origin = &spec.url;
    let (_, url) = canonical(onb, origin)?;
    let wanted = normalize_origin(&url);
    if let Some((name, _)) = config
        .repos
        .iter()
        .find(|(_, other)| normalize_origin(&other.url) == wanted)
    {
        return Err(format!("{origin} is repository {name} already"));
    }
    if let Some(doc) = pending.iter().find(|doc| {
        doc.onboard
            .as_ref()
            .is_some_and(|other| normalize_origin(&other.url) == wanted)
    }) {
        return Err(format!(
            "{origin} is requested as {} in #{}",
            doc.repo, doc.number
        ));
    }
    let branch = &spec.default_branch;
    if !(1..=100).contains(&branch.len())
        || branch.chars().any(char::is_whitespace)
        || branch.starts_with('-')
    {
        return Err("a default branch is one word of 1 to 100 bytes".to_owned());
    }
    if spec.grants.len() > MAX_GRANTS {
        return Err(format!("a request grants at most {MAX_GRANTS} secrets"));
    }
    check_grants(repo, &spec.grants, |upstream| {
        config.repos.contains_key(upstream)
    })?;
    if spec.prerequisites.len() > MAX_PREREQUISITES
        || spec
            .prerequisites
            .iter()
            .any(|line| !(1..=200).contains(&line.len()) || line.contains(['\n', '\r']))
    {
        return Err("a prerequisite is one line of 1 to 200 bytes".to_owned());
    }
    Ok(OnboardSpec {
        url,
        ..spec.clone()
    })
}

/// Works through the approved onboarding requests.
pub struct Onboarder {
    state_dir: PathBuf,
    config: Arc<Shared>,
    secrets: Arc<Secrets>,
    /// The documents' lock, shared with the `/ci` server.
    docs_lock: Arc<Mutex<()>>,
    /// The repositories whose documents or configuration changed, for the
    /// service to announce.
    docs_changed: Sender<String>,
}

impl Onboarder {
    pub fn new(
        state_dir: PathBuf,
        config: Arc<Shared>,
        secrets: Arc<Secrets>,
        docs_lock: Arc<Mutex<()>>,
        docs_changed: Sender<String>,
    ) -> Onboarder {
        Onboarder {
            state_dir,
            config,
            secrets,
            docs_lock,
            docs_changed,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.docs_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Onboards every approved request and records how it went. The forge
    /// and OpenBao are asked without the documents' lock held.
    pub fn run_once(&self, now: u64) {
        let approved = {
            let _held = self.lock();
            docs::approved_onboardings(&self.state_dir)
        };
        for doc in approved {
            let (outcome, changed) = match self.onboard(&doc, now) {
                Ok(changed) => (Ok(()), changed),
                Err(e) => (Err(e), Vec::new()),
            };
            match &outcome {
                Ok(()) => eprintln!("[ci] onboarded {} (#{})", doc.repo, doc.number),
                Err(e) => eprintln!("[ci] onboarding {} #{} failed: {e}", doc.repo, doc.number),
            }
            let finished = {
                let _held = self.lock();
                docs::finish_onboarding(&self.state_dir, &doc.repo, doc.number, outcome, now)
            };
            if let Err(e) = finished {
                eprintln!("[ci] onboarding {} #{}: {e}", doc.repo, doc.number);
            }
            let _ = self.docs_changed.send(doc.repo.clone());
            for repo in changed {
                if repo != doc.repo {
                    let _ = self.docs_changed.send(repo);
                }
            }
        }
    }

    /// The steps of one request; answers the repositories a reload changed.
    fn onboard(&self, doc: &DocView, now: u64) -> Result<Vec<String>, String> {
        let step = |name: &str| {
            let name = name.to_owned();
            move |e: String| format!("{name}: {e}")
        };
        let config = self.config.get();
        let onb = config
            .onboarding
            .as_ref()
            .ok_or("ci.toml has no [onboarding]")
            .map_err(|e| step("setup")(e.to_owned()))?;
        let spec = doc
            .onboard
            .as_ref()
            .ok_or("the request names no repository")
            .map_err(|e| step("setup")(e.to_owned()))?;
        let (slug, _) = canonical(onb, &spec.url).map_err(step("setup"))?;
        let repo = repo_config(onb, &doc.repo, spec, &slug);
        if let Some(existing) = config.repos.get(&doc.repo)
            && normalize_origin(&existing.url) != normalize_origin(&repo.url)
        {
            return Err(step("setup")(format!(
                "{} is in ci.toml with another url",
                doc.repo
            )));
        }

        let fields = self
            .secrets
            .fields()
            .map_err(|e| step("secrets")(e.to_string()))?;
        for name in [&onb.admin_token, &onb.bot_token]
            .into_iter()
            .chain(spec.grants.keys())
        {
            if !fields.contains(name) {
                return Err(step("secrets")(format!(
                    "{name} is not in {}; store it there, then approve #{} again",
                    self.secrets.document(),
                    doc.number
                )));
            }
        }
        let admin = self
            .secrets
            .get(&onb.admin_token)
            .map_err(|e| step("secrets")(e.to_string()))?;

        let repo_api = format!("{}/api/v1/repos/{slug}", onb.api);
        forge::send(
            "PUT",
            &format!("{repo_api}/collaborators/{}", onb.bot),
            &admin,
            Some(r#"{"permission":"write"}"#),
        )
        .map_err(step("collaborator"))?;

        let secret_name = webhook_secret_name(&doc.repo);
        let secret = if fields.contains(&secret_name) {
            self.secrets
                .get(&secret_name)
                .map_err(|e| step("webhook secret")(e.to_string()))?
        } else {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes)
                .map_err(|e| step("webhook secret")(format!("no random bytes: {e}")))?;
            let value = hex::encode(bytes);
            self.secrets
                .patch(None, &[(&secret_name, &value)])
                .map_err(|e| step("webhook secret")(e.to_string()))?;
            value
        };

        let target = format!("{}/{}", onb.hook_url, doc.repo);
        let hooks =
            forge::send("GET", &format!("{repo_api}/hooks"), &admin, None).map_err(step("hook"))?;
        let hooks: Vec<serde_json::Value> = serde_json::from_str(&hooks)
            .map_err(|e| step("hook")(format!("unexpected answer: {e}")))?;
        let existing = hooks.iter().find_map(|hook| {
            (hook.pointer("/config/url").and_then(|u| u.as_str()) == Some(target.as_str()))
                .then(|| hook.get("id").and_then(serde_json::Value::as_u64))
                .flatten()
        });
        let hook_config = serde_json::json!({
            "url": target,
            "content_type": "json",
            "secret": secret,
        });
        match existing {
            Some(id) => forge::send(
                "PATCH",
                &format!("{repo_api}/hooks/{id}"),
                &admin,
                Some(&serde_json::json!({ "config": hook_config, "active": true }).to_string()),
            ),
            None => forge::send(
                "POST",
                &format!("{repo_api}/hooks"),
                &admin,
                Some(
                    &serde_json::json!({
                        "type": "forgejo",
                        "config": hook_config,
                        "events": ["push"],
                        "branch_filter": "*",
                        "active": true,
                    })
                    .to_string(),
                ),
            ),
        }
        .map_err(step("hook"))?;

        let mut changed = Vec::new();
        if !config.repos.contains_key(&doc.repo) {
            let file = onboarded_dir(&self.state_dir).join(format!("{}.toml", doc.repo));
            let text = toml::to_string(&repo).map_err(|e| step("ci.toml")(e.to_string()))?;
            crate::files::write_atomic(&file, text.as_bytes()).map_err(step("ci.toml"))?;
            match self.config.reload(&self.state_dir) {
                Ok(names) => changed = names,
                Err(e) => {
                    let _ = std::fs::remove_file(&file);
                    return Err(step("ci.toml")(e));
                }
            }
        }

        // After the reload: the scheduler refreshes its configuration at the
        // start of the tick that reads this event.
        let event = CiEvent {
            repo: doc.repo.clone(),
            kind: EventKind::Push,
            git_ref: spec.default_branch.clone(),
            sha: None,
            actor: format!("onboarding #{}", doc.number),
            delivery: None,
            upload: None,
            upstream: None,
            received: now,
        };
        event::write_inbox(&self.state_dir, &event).map_err(step("first pipeline"))?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn onboarding() -> OnboardingConfig {
        OnboardingConfig {
            api: "https://git.doodleshnookie.net".to_owned(),
            bot: "zeughaus".to_owned(),
            bot_token: "FORGEJO_TOKEN".to_owned(),
            admin_token: "FORGEJO_ADMIN_TOKEN".to_owned(),
            hook_url: "https://ci.doodleshnookie.net/hook".to_owned(),
        }
    }

    #[test]
    fn an_origin_on_the_forgejo_is_made_canonical() {
        assert_eq!(
            canonical(
                &onboarding(),
                "git@git.doodleshnookie.net:tuco86/Scan-RS.git"
            ),
            Ok((
                "tuco86/scan-rs".to_owned(),
                "https://git.doodleshnookie.net/tuco86/scan-rs.git".to_owned()
            ))
        );
        assert_eq!(
            canonical(&onboarding(), "https://github.com/o/r").unwrap_err(),
            "https://github.com/o/r is not on https://git.doodleshnookie.net; onboarding adds repositories of that Forgejo only"
        );
        assert_eq!(
            canonical(&onboarding(), "https://git.doodleshnookie.net/tuco86").unwrap_err(),
            "https://git.doodleshnookie.net/tuco86 names no owner/name repository on https://git.doodleshnookie.net"
        );
    }

    #[test]
    fn a_webhook_secret_is_named_after_its_repository() {
        assert_eq!(
            webhook_secret_name("weida-site"),
            "WEIDA_SITE_WEBHOOK_SECRET"
        );
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread::JoinHandle;
        use std::time::Duration;

        use serde_json::{Map, Value, json};
        use zeughaus_link::ci::{DocKind, DocState};

        use crate::config::CiConfig;

        const DOCUMENT: &str = "/v1/secret/data/zeughaus/ci";
        const REPO_API: &str = "/api/v1/repos/tuco86/scan-rs";

        /// OpenBao and Forgejo in one: an in-memory document that PATCHes
        /// merge into, and the forge calls onboarding makes, each recorded
        /// as `(method, path, body)`.
        struct StandIn {
            url: String,
            calls: Arc<Mutex<Vec<(String, String, String)>>>,
            stop: Arc<AtomicBool>,
            thread: Option<JoinHandle<()>>,
        }

        impl StandIn {
            fn start(collaborator: u16) -> StandIn {
                let server = tiny_http::Server::http("127.0.0.1:0").expect("bind the stand-in");
                let url = format!("http://{}", server.server_addr().to_ip().expect("an ip"));
                let calls = Arc::new(Mutex::new(Vec::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let thread = {
                    let (calls, stop) = (Arc::clone(&calls), Arc::clone(&stop));
                    std::thread::spawn(move || {
                        let mut document: Map<String, Value> = Map::new();
                        document.insert("FORGEJO_TOKEN".into(), "bot".into());
                        document.insert("FORGEJO_ADMIN_TOKEN".into(), "admin".into());
                        while !stop.load(Ordering::SeqCst) {
                            let Ok(Some(mut request)) =
                                server.recv_timeout(Duration::from_millis(20))
                            else {
                                continue;
                            };
                            let method = request.method().to_string();
                            let path = request.url().to_owned();
                            let mut body = String::new();
                            let _ = request.as_reader().read_to_string(&mut body);
                            let hooks = format!("{REPO_API}/hooks");
                            let collaborators = format!("{REPO_API}/collaborators/zeughaus");
                            let (code, answer) = match (method.as_str(), path.as_str()) {
                                ("POST", "/v1/auth/approle/login") => {
                                    (200, json!({ "auth": { "client_token": "tok" } }))
                                }
                                ("GET", DOCUMENT) => {
                                    (200, json!({ "data": { "data": document.clone() } }))
                                }
                                ("PATCH", DOCUMENT) => {
                                    let patch: Value = serde_json::from_str(&body).unwrap();
                                    for (name, value) in patch["data"].as_object().unwrap() {
                                        document.insert(name.clone(), value.clone());
                                    }
                                    (204, Value::Null)
                                }
                                ("PUT", p) if p == collaborators => (collaborator, Value::Null),
                                ("GET", p) if p == hooks => (200, json!([])),
                                ("POST", p) if p == hooks => (201, json!({ "id": 1 })),
                                _ => (404, Value::Null),
                            };
                            let recorded = !matches!(
                                (method.as_str(), path.as_str()),
                                ("POST", "/v1/auth/approle/login") | ("GET", DOCUMENT)
                            );
                            if recorded {
                                calls.lock().unwrap().push((method, path, body));
                            }
                            let text = if answer.is_null() {
                                String::new()
                            } else {
                                answer.to_string()
                            };
                            let _ = request.respond(
                                tiny_http::Response::from_string(text).with_status_code(code),
                            );
                        }
                    })
                };
                StandIn {
                    url,
                    calls,
                    stop,
                    thread: Some(thread),
                }
            }
        }

        impl Drop for StandIn {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }

        struct Fixture {
            state: PathBuf,
            shared: Arc<Shared>,
            onboarder: Onboarder,
            _changed: std::sync::mpsc::Receiver<String>,
        }

        /// A state directory whose ci.toml points `[secrets]` and
        /// `[onboarding]` at `stand_in`, with request #1 for `scan-rs`
        /// approved.
        fn approved_request(name: &str, stand_in: &StandIn) -> Fixture {
            let state =
                std::env::temp_dir().join(format!("zci-onboard-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&state);
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(
                state.join(crate::config::CONFIG_FILE),
                format!(
                    "default_runner = \"r\"\n[runners.r]\n\
                     fingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n\
                     [secrets]\naddr = \"{url}\"\npath = \"zeughaus/ci\"\nrole_id = \"role\"\n\
                     [onboarding]\napi = \"{url}\"\nbot = \"zeughaus\"\nbot_token = \"FORGEJO_TOKEN\"\n\
                     admin_token = \"FORGEJO_ADMIN_TOKEN\"\nhook_url = \"https://ci.example.org/hook\"\n",
                    url = stand_in.url
                ),
            )
            .unwrap();
            let config = CiConfig::load(&state).unwrap().unwrap();
            let secrets =
                Secrets::with_secret_id(config.secrets.clone().unwrap(), "sid".to_owned());
            let shared = Shared::new(config);
            let spec = OnboardSpec {
                url: format!("{}/tuco86/scan-rs.git", stand_in.url),
                default_branch: "main".to_owned(),
                grants: Default::default(),
                prerequisites: Vec::new(),
            };
            let number = docs::create(
                &state,
                &shared.get(),
                "scan-rs",
                docs::NewDoc {
                    kind: DocKind::Onboarding,
                    title: "Join",
                    body: "Scans.",
                    source: "scan-rs",
                    links: &[],
                    runs_on: &[],
                    onboard: Some(&spec),
                },
                1,
            )
            .unwrap();
            docs::set_state(
                &state,
                &shared.get(),
                "scan-rs",
                number,
                DocState::Approved,
                2,
            )
            .unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let onboarder = Onboarder::new(
                state.clone(),
                Arc::clone(&shared),
                secrets,
                Arc::new(Mutex::new(())),
                tx,
            );
            Fixture {
                state,
                shared,
                onboarder,
                _changed: rx,
            }
        }

        fn stored(state: &Path) -> DocView {
            crate::files::read_json(&crate::ci_dir(state).join("docs/scan-rs/1.json")).unwrap()
        }

        #[test]
        fn approving_registers_the_repository_without_a_restart() {
            let stand_in = StandIn::start(204);
            let f = approved_request("approve", &stand_in);
            f.onboarder.run_once(3);

            let doc = stored(&f.state);
            assert_eq!((doc.state, doc.note.as_str()), (DocState::Done, ""));
            let config = f.shared.get();
            let repo = &config.repos["scan-rs"];
            assert_eq!(
                repo.webhook_secret.as_deref(),
                Some("SCAN_RS_WEBHOOK_SECRET")
            );
            assert_eq!(repo.slug, "tuco86/scan-rs");
            assert_eq!(repo.fetch_user, "zeughaus");

            let calls = stand_in.calls.lock().unwrap().clone();
            let order: Vec<(&str, &str)> = calls
                .iter()
                .map(|(method, path, _)| (method.as_str(), path.as_str()))
                .collect();
            assert_eq!(
                order,
                [
                    ("PUT", "/api/v1/repos/tuco86/scan-rs/collaborators/zeughaus"),
                    ("PATCH", DOCUMENT),
                    ("GET", "/api/v1/repos/tuco86/scan-rs/hooks"),
                    ("POST", "/api/v1/repos/tuco86/scan-rs/hooks"),
                ]
            );
            let patch: Value = serde_json::from_str(&calls[1].2).unwrap();
            let secret = patch["data"]["SCAN_RS_WEBHOOK_SECRET"].as_str().unwrap();
            assert_eq!(secret.len(), 64);
            assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
            let hook: Value = serde_json::from_str(&calls[3].2).unwrap();
            assert_eq!(hook["config"]["secret"], secret);
            assert_eq!(hook["config"]["url"], "https://ci.example.org/hook/scan-rs");
            assert_eq!(hook["type"], "forgejo");
            let collaborator: Value = serde_json::from_str(&calls[0].2).unwrap();
            assert_eq!(collaborator, json!({ "permission": "write" }));

            crate::event::drain_inbox(&f.state);
            let queued: Vec<(String, EventKind, String)> = crate::event::queued(&f.state)
                .into_iter()
                .map(|(_, e)| (e.repo, e.kind, e.git_ref))
                .collect();
            assert_eq!(
                queued,
                [("scan-rs".to_owned(), EventKind::Push, "main".to_owned())]
            );
            // A configuration read from disk has it too.
            assert!(
                CiConfig::load(&f.state)
                    .unwrap()
                    .unwrap()
                    .repos
                    .contains_key("scan-rs")
            );
            let _ = std::fs::remove_dir_all(&f.state);
        }

        #[test]
        fn a_refused_collaborator_returns_the_request_with_its_note() {
            let stand_in = StandIn::start(403);
            let f = approved_request("refused", &stand_in);
            f.onboarder.run_once(3);

            let doc = stored(&f.state);
            assert_eq!(doc.state, DocState::Open);
            assert!(doc.note.starts_with("collaborator: "), "{}", doc.note);
            assert!(!onboarded_dir(&f.state).join("scan-rs.toml").exists());
            assert!(!f.shared.get().repos.contains_key("scan-rs"));
            let _ = std::fs::remove_dir_all(&f.state);
        }
    }
}
