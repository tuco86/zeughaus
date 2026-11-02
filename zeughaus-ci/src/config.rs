//! `<state-dir>/ci.toml`.
//!
//! The config only ever names secrets; their values live in OpenBao
//! (`[secrets]`, read by `crate::secrets`), so `ci.toml` holds nothing that
//! must stay private.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use zeughaus_link::ci::normalize_origin;
use zeughaus_link::enlist::Offer;
use zeughaus_link::selector::valid_label;

use crate::event::CiEvent;
use crate::pipeline::Pattern;

pub const CONFIG_FILE: &str = "ci.toml";

/// `ci/onboarded/<name>.toml`: one `[repos.<name>]` table each, written by
/// the CI when it onboards a repository.
pub const ONBOARDED_DIR: &str = "onboarded";

pub fn onboarded_dir(state_dir: &Path) -> PathBuf {
    crate::ci_dir(state_dir).join(ONBOARDED_DIR)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiConfig {
    /// The runner of jobs that name no `machine`.
    pub default_runner: String,
    /// Compressed blobs the artifact store under `ci/blobs` may hold, in GiB.
    #[serde(default = "default_artifact_store_gb")]
    pub artifact_store_gb: u64,
    /// The largest version of one job state store (`state = [..]` in a job
    /// header), as the sum of its file sizes, in MiB.
    #[serde(default = "default_state_max_mb")]
    pub state_max_mb: u64,
    /// Runners that may enlist, by name.
    #[serde(default)]
    pub runners: BTreeMap<String, RunnerConfig>,
    /// Platform name -> the runner or machine that builds it.
    #[serde(default)]
    pub platforms: BTreeMap<String, String>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoConfig>,
    /// Where the values of the secrets named below live.
    pub secrets: Option<SecretsConfig>,
    /// When the CI runs every repository's nightly; no round without it.
    #[serde(default)]
    pub nightly: Option<NightlyConfig>,
    /// How the CI onboards a repository; no onboarding requests without it.
    #[serde(default)]
    pub onboarding: Option<OnboardingConfig>,
    /// The language model that advises on projects the CI does not build;
    /// no advice without it.
    #[serde(default)]
    pub advice: Option<AdviceConfig>,
}

/// `[advice]`: the Anthropic Messages API the CI asks for advice on a
/// project that has not joined, and the secret it asks with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdviceConfig {
    /// Secret name: an API key (`x-api-key`), or an OAuth access token
    /// (`sk-ant-oat...`, sent as a bearer token).
    pub key: String,
    #[serde(default = "default_advice_model")]
    pub model: String,
    /// The API base; a trailing `/` is trimmed.
    #[serde(default = "default_advice_api")]
    pub api: String,
}

fn default_advice_model() -> String {
    "claude-haiku-5-5".into()
}

fn default_advice_api() -> String {
    "https://api.anthropic.com".into()
}

/// `[onboarding]`: the Forgejo the CI adds repositories of, and the names
/// of the secrets it does that with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingConfig {
    /// Forgejo base, e.g. `https://git.doodleshnookie.net`; a trailing `/`
    /// is trimmed.
    pub api: String,
    /// The collaborator added with write access, e.g. `zeughaus-ci`.
    pub bot: String,
    /// Secret name: the `fetch_token` and `status_token` of an onboarded
    /// repository.
    pub bot_token: String,
    /// Secret name: the owner-level token that adds the collaborator and the
    /// hook.
    pub admin_token: String,
    /// e.g. `https://ci.doodleshnookie.net/hook`; the hook of repository
    /// `<name>` is `{hook_url}/{name}`.
    pub hook_url: String,
}

/// `[nightly]`: the CI-wide time of the nightly round.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NightlyConfig {
    /// `HH:MM` in the CI host's local time. A time inside a skipped DST
    /// hour does not occur that day.
    at: String,
    /// `at`, parsed by `validate`.
    #[serde(skip)]
    pub time: chrono::NaiveTime,
}

/// `HH:MM`, both parts exactly two ASCII digits, 00:00 to 23:59.
pub fn parse_clock(text: &str) -> Option<chrono::NaiveTime> {
    let (hours, minutes) = text.split_once(':')?;
    let two_digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(hours) || !two_digits(minutes) {
        return None;
    }
    chrono::NaiveTime::from_hms_opt(hours.parse().ok()?, minutes.parse().ok()?, 0)
}

fn default_artifact_store_gb() -> u64 {
    2
}

fn default_state_max_mb() -> u64 {
    1024
}

fn default_mount() -> String {
    "secret".into()
}

fn default_cache_ttl() -> u64 {
    30
}

fn default_bao_timeout() -> u64 {
    5
}

/// One KV v2 document in OpenBao, read with the runner's own AppRole.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// `http://10.8.0.1:8200`; a trailing `/` is trimmed.
    pub addr: String,
    /// KV v2 mount.
    #[serde(default = "default_mount")]
    pub mount: String,
    /// Document path below the mount, e.g. `zeughaus/ci`.
    pub path: String,
    /// The AppRole's role_id; not secret, the secret_id is sealed apart.
    pub role_id: String,
    /// How long a read document answers from memory.
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
    /// curl's `--max-time` per request.
    #[serde(default = "default_bao_timeout")]
    pub timeout_seconds: u64,
}

fn default_wait_minutes() -> u32 {
    15
}

/// A runner that may enlist, pinned by the fingerprint of its certificate.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub fingerprint: String,
    /// What the runner's host is, which decides the jobs it runs there.
    #[serde(default)]
    pub os: RunnerOs,
    /// Machines (Windows VMs) this runner boots for jobs with `machine = <name>`.
    #[serde(default)]
    pub machines: Vec<String>,
    /// How long a job waits for this runner to connect before it is skipped.
    #[serde(default = "default_wait_minutes")]
    pub wait_minutes: u32,
    /// Free labels of this runner (`gpu`, `cuda=12`); `os`, `arch` and
    /// `runner` are the CI's own.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The runner may host agent sessions.
    #[serde(default)]
    pub sessions: bool,
    /// The ssh destination a session uses to reach this machine.
    #[serde(default)]
    pub ssh: Option<String>,
}

/// A runner's host: unix runs `.sh` jobs and offers containers and VMs, a
/// Windows runner runs `.ps1` jobs on its own host and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerOs {
    #[default]
    Unix,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Forge {
    Github,
    Forgejo,
    #[default]
    None,
}

fn default_branch() -> String {
    "main".into()
}

fn default_fetch_user() -> String {
    "x-access-token".into()
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub url: String,
    #[serde(default)]
    pub forge: Forge,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub api: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    pub fetch_token: Option<String>,
    #[serde(default = "default_fetch_user")]
    pub fetch_user: String,
    pub status_token: Option<String>,
    pub webhook_secret: Option<String>,
    /// Secret name to the event patterns it is released to.
    #[serde(default)]
    pub grants: BTreeMap<String, Vec<String>>,
    /// Jobs of this repository may request agent sessions (`agent-session.md`
    /// in `CI_OUTPUT`), which the CI files as approved plans.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_sessions: bool,
}

impl RepoConfig {
    /// Whether `secret` may be handed to a job run for `event`.
    pub fn grants(&self, secret: &str, event: &CiEvent) -> bool {
        self.grants.get(secret).is_some_and(|patterns| {
            patterns
                .iter()
                .any(|p| Pattern::parse(p).is_ok_and(|p| p.matches(event)))
        })
    }

    /// Every secret this repo names: its tokens and the keys of `grants`.
    pub fn secret_names(&self) -> impl Iterator<Item = &String> {
        self.fetch_token
            .iter()
            .chain(&self.status_token)
            .chain(&self.webhook_secret)
            .chain(self.grants.keys())
    }
}

impl CiConfig {
    /// The runner whose pinned fingerprint is `fingerprint`.
    pub fn runner_of_fingerprint(&self, fingerprint: &str) -> Option<&str> {
        let wanted = fingerprint.parse::<weida::Fingerprint>().ok()?.to_string();
        self.runners
            .iter()
            .find(|(_, runner)| runner.fingerprint == wanted)
            .map(|(name, _)| name.as_str())
    }

    pub fn is_runner_fingerprint(&self, fingerprint: &str) -> bool {
        self.runner_of_fingerprint(fingerprint).is_some()
    }

    /// The host of runner `runner`; one that is not configured counts as unix.
    pub fn runner_os(&self, runner: &str) -> RunnerOs {
        self.runners
            .get(runner)
            .map_or(RunnerOs::Unix, |runner| runner.os)
    }

    /// The runner that offers machine `machine`.
    pub fn runner_of_machine(&self, machine: &str) -> Option<&str> {
        self.runners
            .iter()
            .find(|(_, runner)| runner.machines.iter().any(|m| m == machine))
            .map(|(name, _)| name.as_str())
    }

    /// Runner `name`'s labels: the configured ones, `runner=<name>`, and
    /// `os=` and `arch=` once its offer is known. Sorted.
    pub fn labels_of(&self, name: &str, offer: Option<&Offer>) -> Vec<String> {
        let mut labels: Vec<String> = self
            .runners
            .get(name)
            .map(|r| r.labels.clone())
            .unwrap_or_default();
        labels.push(format!("runner={name}"));
        if let Some(offer) = offer {
            labels.push(format!("os={}", offer.os));
            let arch = match offer.arch.as_str() {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                other => other,
            };
            labels.push(format!("arch={arch}"));
        }
        labels.sort();
        labels
    }

    /// `ci.toml`'s text alone.
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<CiConfig, String> {
        Self::from_parts(text, &[])
    }

    /// `ci.toml`'s text plus the onboarded repositories, `(name, text)` of
    /// each `ci/onboarded/<name>.toml`, validated as one configuration.
    pub fn from_parts(text: &str, onboarded: &[(String, String)]) -> Result<CiConfig, String> {
        let mut config: CiConfig =
            toml::from_str(text).map_err(|e| format!("{CONFIG_FILE}: {e}"))?;
        for (name, text) in onboarded {
            let file = format!("ci/{ONBOARDED_DIR}/{name}.toml");
            if !valid_name(name) {
                return Err(format!("{file}: the file name is no repository name"));
            }
            let repo: RepoConfig = toml::from_str(text).map_err(|e| format!("{file}: {e}"))?;
            if config.repos.contains_key(name) {
                return Err(format!("{file}: ci.toml defines repository {name} already"));
            }
            let origin = normalize_origin(&repo.url);
            if let Some(other) = config
                .repos
                .iter()
                .find(|(_, other)| normalize_origin(&other.url) == origin)
                .map(|(other, _)| other)
            {
                return Err(format!("{file}: its url is repository {other}'s"));
            }
            config.repos.insert(name.clone(), repo);
        }
        config.validate()?;
        Ok(config)
    }

    /// `Ok(None)` when there is no `ci.toml`: CI is off. The onboarded
    /// repositories under `ci/onboarded/` are read in name order.
    pub fn load(state_dir: &Path) -> Result<Option<CiConfig>, String> {
        let path = state_dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let dir = onboarded_dir(state_dir);
        let mut onboarded = Vec::new();
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
                    let file = entry.path();
                    // `write_atomic` leaves `<name>.toml.tmp` behind a crash.
                    let Some(name) = file
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_suffix(".toml"))
                    else {
                        continue;
                    };
                    let text = std::fs::read_to_string(&file)
                        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
                    onboarded.push((name.to_owned(), text));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
        }
        onboarded.sort();
        Self::from_parts(&text, &onboarded).map(Some)
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.artifact_store_gb == 0 {
            return Err("ci.toml: `artifact_store_gb` must be at least 1".into());
        }
        // The upper bound is the most one artifact transfer carries.
        if !(1..=4096).contains(&self.state_max_mb) {
            return Err("ci.toml: `state_max_mb` must be 1 to 4096".into());
        }
        if self.runners.is_empty() {
            return Err("ci.toml: at least one [runners.<name>] is required".into());
        }
        if !self.runners.contains_key(&self.default_runner) {
            return Err(format!(
                "default_runner `{}` is not a key of [runners]",
                self.default_runner
            ));
        }
        let mut machines: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, runner) in &mut self.runners {
            if !valid_name(name) {
                return Err(format!(
                    "runner name `{name}` must match [a-z0-9][a-z0-9-]*"
                ));
            }
            let fingerprint = runner
                .fingerprint
                .parse::<weida::Fingerprint>()
                .map_err(|e| format!("runner `{name}`: bad fingerprint: {e}"))?;
            // The principal of a connection is `Fingerprint::to_string`, so the
            // stored form must be that spelling.
            runner.fingerprint = fingerprint.to_string();
            if runner.wait_minutes == 0 {
                return Err(format!(
                    "runner `{name}`: `wait_minutes` must be at least 1"
                ));
            }
            if runner.os == RunnerOs::Windows && !runner.machines.is_empty() {
                return Err(format!(
                    "runner `{name}`: a Windows runner boots no machines"
                ));
            }
            for label in &runner.labels {
                if !valid_label(label) {
                    return Err(format!(
                        "runner {name}: label {label} is not key or key=value"
                    ));
                }
                let key = label.split_once('=').map_or(label.as_str(), |(k, _)| k);
                if matches!(key, "os" | "arch" | "runner") {
                    return Err(format!(
                        "runner {name}: label {label}: os, arch and runner are set by the CI"
                    ));
                }
            }
            if let Some(ssh) = &runner.ssh
                && (ssh.is_empty() || ssh.chars().any(char::is_whitespace))
            {
                return Err(format!(
                    "runner {name}: `ssh` must be a destination without whitespace"
                ));
            }
        }
        for (name, runner) in &self.runners {
            for machine in &runner.machines {
                if !valid_name(machine) {
                    return Err(format!(
                        "machine name `{machine}` must match [a-z0-9][a-z0-9-]*"
                    ));
                }
                if self.runners.contains_key(machine) {
                    return Err(format!(
                        "machine `{machine}` of runner `{name}` is also a runner name"
                    ));
                }
                if let Some(other) = machines.insert(machine, name) {
                    return Err(format!(
                        "machine `{machine}` is listed by runners `{other}` and `{name}`"
                    ));
                }
            }
        }
        for (platform, target) in &self.platforms {
            if !valid_name(platform) {
                return Err(format!(
                    "platform name `{platform}` must match [a-z0-9][a-z0-9-]*"
                ));
            }
            if !self.runners.contains_key(target) && !machines.contains_key(target.as_str()) {
                return Err(format!(
                    "platform `{platform}` names `{target}`, which is neither a runner nor a machine of one"
                ));
            }
        }
        if let Some(nightly) = &mut self.nightly {
            nightly.time = parse_clock(&nightly.at).ok_or_else(|| {
                format!(
                    "[nightly]: `at` must be HH:MM (00:00 to 23:59), not `{}`",
                    nightly.at
                )
            })?;
        }
        if let Some(secrets) = &mut self.secrets {
            secrets.addr = secrets.addr.trim_end_matches('/').to_string();
            for (field, value) in [
                ("addr", &secrets.addr),
                ("path", &secrets.path),
                ("role_id", &secrets.role_id),
            ] {
                if value.is_empty() {
                    return Err(format!("[secrets]: `{field}` must not be empty"));
                }
            }
        }
        if let Some(onboarding) = &mut self.onboarding {
            onboarding.api = onboarding.api.trim_end_matches('/').to_string();
            onboarding.hook_url = onboarding.hook_url.trim_end_matches('/').to_string();
            for (field, value) in [
                ("api", &onboarding.api),
                ("bot", &onboarding.bot),
                ("bot_token", &onboarding.bot_token),
                ("admin_token", &onboarding.admin_token),
                ("hook_url", &onboarding.hook_url),
            ] {
                if value.is_empty() {
                    return Err(format!("[onboarding]: `{field}` must not be empty"));
                }
            }
            if onboarding
                .bot
                .chars()
                .any(|c| c == '/' || c.is_whitespace())
            {
                return Err("[onboarding]: `bot` is a user name without `/` or whitespace".into());
            }
            for (field, value) in [
                ("bot_token", &onboarding.bot_token),
                ("admin_token", &onboarding.admin_token),
            ] {
                if !valid_env_name(value) {
                    return Err(format!(
                        "[onboarding]: `{field}` names secret `{value}`, which must match [A-Z_][A-Z0-9_]*"
                    ));
                }
            }
            if self.secrets.is_none() {
                return Err("[onboarding] names secrets but ci.toml has no [secrets]".into());
            }
        }
        if let Some(advice) = &mut self.advice {
            advice.api = advice.api.trim_end_matches('/').to_string();
            if !valid_env_name(&advice.key) {
                return Err(format!(
                    "[advice]: `key` names secret `{}`, which must match [A-Z_][A-Z0-9_]*",
                    advice.key
                ));
            }
            if advice.model.is_empty() || advice.api.is_empty() {
                return Err("[advice]: `model` and `api` must not be empty".into());
            }
            if self.secrets.is_none() {
                return Err("[advice] names a secret but ci.toml has no [secrets]".into());
            }
        }
        let repo_names: BTreeSet<String> = self.repos.keys().cloned().collect();
        for (name, repo) in &mut self.repos {
            if !valid_name(name) {
                return Err(format!("repo name `{name}` must match [a-z0-9][a-z0-9-]*"));
            }
            let mut names_secrets = false;
            for secret in repo.secret_names() {
                names_secrets = true;
                if !valid_env_name(secret) {
                    return Err(format!(
                        "repo `{name}`: secret name `{secret}` must match [A-Z_][A-Z0-9_]*"
                    ));
                }
            }
            if names_secrets && self.secrets.is_none() {
                return Err(format!(
                    "repo `{name}` names secrets but ci.toml has no [secrets]"
                ));
            }
            check_grants(name, &repo.grants, |upstream| repo_names.contains(upstream))?;
            if repo.forge != Forge::None && (repo.slug.is_empty() || repo.api.is_empty()) {
                return Err(format!("repo `{name}`: a forge needs `slug` and `api`"));
            }
            repo.api = repo.api.trim_end_matches('/').to_string();
        }
        Ok(())
    }
}

/// The secret names and event patterns of repository `name`'s grants; an
/// upstream pattern must name a repository `known` accepts.
pub fn check_grants(
    name: &str,
    grants: &BTreeMap<String, Vec<String>>,
    known: impl Fn(&str) -> bool,
) -> Result<(), String> {
    for (secret, patterns) in grants {
        if !valid_env_name(secret) {
            return Err(format!(
                "repo `{name}`: secret name `{secret}` must match [A-Z_][A-Z0-9_]*"
            ));
        }
        for pattern in patterns {
            let parsed = Pattern::parse(pattern)
                .map_err(|e| format!("repo `{name}`: grant for {secret}: {e}"))?;
            if let Pattern::Upstream { repo: upstream, .. } = &parsed
                && !known(upstream)
            {
                return Err(format!(
                    "repo `{name}`: grant for {secret}: `{pattern}` names `{upstream}`, which is no repository of ci.toml"
                ));
            }
        }
    }
    Ok(())
}

/// The configuration every service reads, swapped whole when the CI
/// reloads `[repos]` (SIGHUP, or an onboarding). A reader takes one
/// snapshot per request or loop turn and keeps it for that turn.
#[derive(Debug)]
pub struct Shared {
    current: RwLock<Arc<CiConfig>>,
}

impl Shared {
    pub fn new(config: CiConfig) -> Arc<Shared> {
        Arc::new(Shared {
            current: RwLock::new(Arc::new(config)),
        })
    }

    pub fn get(&self) -> Arc<CiConfig> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Swaps in `next` when it differs from the current configuration in
    /// `[repos]` only, and answers the sorted names of the repositories that
    /// were added, removed or changed.
    pub fn replace(&self, next: CiConfig) -> Result<Vec<String>, String> {
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        let without_repos = |config: &CiConfig| CiConfig {
            repos: BTreeMap::new(),
            ..config.clone()
        };
        if without_repos(&current) != without_repos(&next) {
            return Err("a change outside [repos] takes a restart".into());
        }
        let changed: BTreeSet<String> = current
            .repos
            .keys()
            .chain(next.repos.keys())
            .filter(|name| current.repos.get(*name) != next.repos.get(*name))
            .cloned()
            .collect();
        *current = Arc::new(next);
        Ok(changed.into_iter().collect())
    }

    /// [`CiConfig::load`] and [`Shared::replace`].
    pub fn reload(&self, state_dir: &Path) -> Result<Vec<String>, String> {
        let next = CiConfig::load(state_dir)?.ok_or("no ci.toml")?;
        self.replace(next)
    }
}

/// `[a-z0-9][a-z0-9-]*`
pub fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `[A-Z_][A-Z0-9_]*`
pub fn valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    const FP_A: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    const FP_B: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";

    const EXAMPLE: &str = r#"
default_runner = "tuco-pc"
[runners.tuco-pc]
fingerprint = "sha256:0000000000000000000000000000000000000000000000000000000000000001"
machines = ["win11"]
[runners.atik]
fingerprint = "sha256:0000000000000000000000000000000000000000000000000000000000000002"
wait_minutes = 30
[repos.griasdi]
url = "https://github.com/Griasdi/Griasdi.git"
forge = "github"
slug = "Griasdi/Griasdi"
api = "https://api.github.com/"
default_branch = "main"
fetch_token = "GITHUB_TOKEN"
fetch_user = "x-access-token"
status_token = "GITHUB_TOKEN"
webhook_secret = "GRIASDI_WEBHOOK_SECRET"
[repos.griasdi.grants]
GITHUB_TOKEN = ["tag v*", "nightly"]
[secrets]
addr = "http://10.8.0.1:8200/"
path = "zeughaus/ci"
role_id = "r-1"
[repos.smoke]
url = "/var/lib/zeughaus-ci/smoke.git"
forge = "none"
"#;

    fn with_runners(extra: &str) -> String {
        format!("default_runner = \"a\"\n[runners.a]\nfingerprint = \"{FP_A}\"\n{extra}")
    }

    fn event(kind: EventKind, git_ref: &str) -> CiEvent {
        CiEvent {
            repo: "griasdi".into(),
            kind,
            git_ref: git_ref.into(),
            sha: None,
            actor: "t".into(),
            delivery: None,
            upload: None,
            upstream: None,
            received: 0,
        }
    }

    #[test]
    fn parses_the_example() {
        let config = CiConfig::parse(EXAMPLE).unwrap();
        assert_eq!(config.default_runner, "tuco-pc");
        assert_eq!(config.runners.len(), 2);
        assert_eq!(config.runners["tuco-pc"].machines, ["win11"]);
        assert_eq!(config.runners["tuco-pc"].wait_minutes, 15);
        assert_eq!(config.runners["atik"].wait_minutes, 30);
        assert_eq!(config.runner_of_fingerprint(FP_B), Some("atik"));
        assert!(config.is_runner_fingerprint(FP_A));
        assert!(!config.is_runner_fingerprint("sha256:nope"));
        assert_eq!(config.runner_of_machine("win11"), Some("tuco-pc"));
        let griasdi = &config.repos["griasdi"];
        assert_eq!(griasdi.forge, Forge::Github);
        assert_eq!(griasdi.api, "https://api.github.com");
        assert_eq!(griasdi.fetch_user, "x-access-token");
        let smoke = &config.repos["smoke"];
        assert_eq!(smoke.forge, Forge::None);
        assert_eq!(smoke.default_branch, "main");
        assert!(smoke.fetch_token.is_none());
    }

    #[test]
    fn deploy_example_parses() {
        let text = include_str!("../../deploy/ci/ci.toml.example");
        // The fingerprints in the example are placeholders; swap them for valid ones.
        let text = text.replace("<fingerprint>", FP_A);
        CiConfig::parse(&text).unwrap();
    }

    #[test]
    fn default_runner_is_required_and_must_exist() {
        assert!(CiConfig::parse("").is_err());
        let err = CiConfig::parse(&format!(
            "default_runner = \"b\"\n[runners.a]\nfingerprint = \"{FP_A}\""
        ))
        .unwrap_err();
        assert!(err.contains("default_runner"), "{err}");
        assert!(CiConfig::parse(&with_runners("")).is_ok());
    }

    #[test]
    fn artifact_store_budget() {
        assert_eq!(
            CiConfig::parse(&with_runners(""))
                .unwrap()
                .artifact_store_gb,
            2
        );
        let text = format!("artifact_store_gb = 100\n{}", with_runners(""));
        assert_eq!(CiConfig::parse(&text).unwrap().artifact_store_gb, 100);
        let text = format!("artifact_store_gb = 0\n{}", with_runners(""));
        let err = CiConfig::parse(&text).unwrap_err();
        assert_eq!(err, "ci.toml: `artifact_store_gb` must be at least 1");
    }

    #[test]
    fn state_size_limit() {
        assert_eq!(
            CiConfig::parse(&with_runners("")).unwrap().state_max_mb,
            1024
        );
        for bad in [0, 4097] {
            let text = format!("state_max_mb = {bad}\n{}", with_runners(""));
            let err = CiConfig::parse(&text).unwrap_err();
            assert_eq!(err, "ci.toml: `state_max_mb` must be 1 to 4096");
        }
        let text = format!("state_max_mb = 4096\n{}", with_runners(""));
        assert_eq!(CiConfig::parse(&text).unwrap().state_max_mb, 4096);
    }

    #[test]
    fn machine_names_are_unique_and_not_runner_names() {
        let err = CiConfig::parse(&with_runners(&format!(
            "machines = [\"a\"]\n[runners.b]\nfingerprint = \"{FP_B}\"\n"
        )))
        .unwrap_err();
        assert!(err.contains("machine `a`"), "{err}");
        let err = CiConfig::parse(&with_runners(&format!(
            "machines = [\"m\"]\n[runners.b]\nfingerprint = \"{FP_B}\"\nmachines = [\"m\"]\n"
        )))
        .unwrap_err();
        assert!(err.contains("machine `m`"), "{err}");
    }

    #[test]
    fn platforms_name_a_runner_or_a_machine() {
        let text = with_runners(
            "machines = [\"w\"]\n[platforms]\nlinux-amd64 = \"a\"\nwindows-amd64 = \"w\"\n",
        );
        let config = CiConfig::parse(&text).unwrap();
        assert_eq!(config.platforms["linux-amd64"], "a");
        assert_eq!(config.platforms["windows-amd64"], "w");

        let err =
            CiConfig::parse(&with_runners("[platforms]\nlinux-amd64 = \"nowhere\"\n")).unwrap_err();
        assert!(
            err.contains("platform `linux-amd64` names `nowhere`"),
            "{err}"
        );
        let err = CiConfig::parse(&with_runners("[platforms]\nLinux = \"a\"\n")).unwrap_err();
        assert!(err.contains("platform name `Linux`"), "{err}");
    }

    #[test]
    fn runner_validation() {
        let bad_fp = "default_runner = \"a\"\n[runners.a]\nfingerprint = \"nope\"";
        assert!(CiConfig::parse(bad_fp).is_err());
        let bad_name = format!("default_runner = \"A\"\n[runners.A]\nfingerprint = \"{FP_A}\"");
        assert!(CiConfig::parse(&bad_name).is_err());
        assert!(CiConfig::parse(&with_runners("machines = [\"W\"]")).is_err());
    }

    #[test]
    fn runner_labels_are_checked_and_the_ci_adds_its_own() {
        let config = CiConfig::parse(&with_runners(
            "labels = [\"gpu\", \"cuda=12.4\"]\nsessions = true\nssh = \"me@10.0.0.1\"\n",
        ))
        .unwrap();
        assert!(config.runners["a"].sessions);
        assert_eq!(config.runners["a"].ssh.as_deref(), Some("me@10.0.0.1"));
        let offer = Offer {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            cpus: 1,
            containers: false,
            machines: Vec::new(),
        };
        assert_eq!(
            config.labels_of("a", Some(&offer)),
            ["arch=amd64", "cuda=12.4", "gpu", "os=linux", "runner=a"]
        );
        assert_eq!(
            config.labels_of("a", None),
            ["cuda=12.4", "gpu", "runner=a"]
        );
        let arm = Offer {
            arch: "aarch64".to_owned(),
            os: "macos".to_owned(),
            ..offer.clone()
        };
        assert!(
            config
                .labels_of("a", Some(&arm))
                .contains(&"arch=arm64".to_owned())
        );
        let riscv = Offer {
            arch: "riscv64".to_owned(),
            ..offer
        };
        assert!(
            config
                .labels_of("a", Some(&riscv))
                .contains(&"arch=riscv64".to_owned())
        );

        let err = CiConfig::parse(&with_runners("labels = [\"Gpu\"]\n")).unwrap_err();
        assert_eq!(err, "runner a: label Gpu is not key or key=value");
        let err = CiConfig::parse(&with_runners("labels = [\"os=linux\"]\n")).unwrap_err();
        assert_eq!(
            err,
            "runner a: label os=linux: os, arch and runner are set by the CI"
        );
        assert!(CiConfig::parse(&with_runners("labels = [\"runner\"]\n")).is_err());
        assert!(CiConfig::parse(&with_runners("ssh = \"\"\n")).is_err());
        assert!(CiConfig::parse(&with_runners("ssh = \"a b\"\n")).is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(CiConfig::parse(&format!("bogus = 1\n{}", with_runners(""))).is_err());
        assert!(CiConfig::parse("budget_gb = 50").is_err());
        let err = CiConfig::parse(&with_runners("nope = 1")).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        let base = with_runners("");
        assert!(CiConfig::parse(&format!("{base}[repos.a]\nurl = \"x\"\nnope = 1")).is_err());
    }

    #[test]
    fn validation() {
        let parse = |extra: &str| CiConfig::parse(&format!("{}{extra}", with_runners("")));
        assert!(parse("[repos.Bad]\nurl = \"x\"").is_err());
        assert!(parse("[repos.a]\nurl = \"x\"\nfetch_token = \"lower\"").is_err());
        assert!(parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nlower = []").is_err());
        assert!(parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nA = [\"cron *\"]").is_err());
        assert!(parse("[repos.a]\nurl = \"x\"\nforge = \"github\"").is_err());
    }

    #[test]
    fn secrets_need_a_source() {
        let parse = |extra: &str| CiConfig::parse(&format!("{}{extra}", with_runners("")));
        let err = parse("[repos.a]\nurl = \"x\"\nfetch_token = \"TOKEN\"").unwrap_err();
        assert_eq!(err, "repo `a` names secrets but ci.toml has no [secrets]");
        let err = parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nTOKEN = []").unwrap_err();
        assert_eq!(err, "repo `a` names secrets but ci.toml has no [secrets]");
        assert!(parse("[secrets]\naddr = \"a\"\npath = \"p\"\nrole_id = \"\"").is_err());
        let config =
            parse("[secrets]\naddr = \"http://b:8200/\"\npath = \"p\"\nrole_id = \"r\"").unwrap();
        let secrets = config.secrets.unwrap();
        assert_eq!(secrets.addr, "http://b:8200");
        assert_eq!(secrets.mount, "secret");
        assert_eq!(secrets.cache_ttl_seconds, 30);
    }

    #[test]
    fn grants_match_events() {
        let config = CiConfig::parse(EXAMPLE).unwrap();
        let repo = &config.repos["griasdi"];
        assert!(repo.grants("GITHUB_TOKEN", &event(EventKind::Tag, "v1.2")));
        assert!(!repo.grants("GITHUB_TOKEN", &event(EventKind::Tag, "x1")));
        assert!(!repo.grants("GITHUB_TOKEN", &event(EventKind::Push, "main")));
        let nightly = event(EventKind::Nightly, "main");
        assert!(repo.grants("GITHUB_TOKEN", &nightly));
        assert!(!repo.grants("OTHER", &nightly));
    }

    #[test]
    fn an_upstream_grant_releases_a_secret_to_the_followed_push() {
        let grant = |pattern: &str| {
            CiConfig::parse(&format!(
                "{EXAMPLE}[repos.smoke.grants]\nGITHUB_TOKEN = [\"{pattern}\"]\n"
            ))
        };
        let config = grant("push main of griasdi").unwrap();
        let smoke = &config.repos["smoke"];
        let follow = |git_ref: &str| CiEvent {
            repo: "smoke".into(),
            upstream: Some(crate::event::Upstream {
                repo: "griasdi".into(),
                git_ref: git_ref.into(),
                sha: "a".into(),
            }),
            ..event(EventKind::Upstream, "main")
        };
        assert!(smoke.grants("GITHUB_TOKEN", &follow("main")));
        assert!(!smoke.grants("GITHUB_TOKEN", &follow("dev")));
        let push = CiEvent {
            repo: "smoke".into(),
            ..event(EventKind::Push, "main")
        };
        assert!(!smoke.grants("GITHUB_TOKEN", &push));
        assert_eq!(
            grant("push main of nope").unwrap_err(),
            "repo `smoke`: grant for GITHUB_TOKEN: `push main of nope` names `nope`, which is no repository of ci.toml"
        );
    }

    #[test]
    fn the_nightly_round_is_at_a_clock_time() {
        let parse = |extra: &str| CiConfig::parse(&format!("{}{extra}", with_runners("")));
        assert!(parse("").unwrap().nightly.is_none());
        let config = parse("[nightly]\nat = \"03:00\"").unwrap();
        assert_eq!(
            config.nightly.unwrap().time,
            chrono::NaiveTime::from_hms_opt(3, 0, 0).unwrap()
        );
        for at in ["3:00", "24:00", "03:60", "0a:00", "+3:00"] {
            let err = parse(&format!("[nightly]\nat = \"{at}\"")).unwrap_err();
            assert_eq!(
                err,
                format!("[nightly]: `at` must be HH:MM (00:00 to 23:59), not `{at}`")
            );
        }
        assert!(parse("[nightly]\nat = \"03:00\"\nevery = 1").is_err());
    }

    #[test]
    fn names() {
        assert!(valid_name("a-b1"));
        assert!(valid_name("1a"));
        assert!(!valid_name(""));
        assert!(!valid_name("-a"));
        assert!(!valid_name("A"));
        assert!(!valid_name("a_b"));
        assert!(valid_env_name("_X1"));
        assert!(valid_env_name("GITHUB_TOKEN"));
        assert!(!valid_env_name("1X"));
        assert!(!valid_env_name("x"));
        assert!(!valid_env_name(""));
    }

    fn drop_in(name: &str, text: &str) -> Vec<(String, String)> {
        vec![(name.to_owned(), text.to_owned())]
    }

    #[test]
    fn an_onboarded_repository_joins_the_configuration() {
        let config = CiConfig::from_parts(
            EXAMPLE,
            &drop_in(
                "scan-rs",
                "url = \"https://git.example/o/scan-rs.git\"\n[grants]\nGITHUB_TOKEN = [\"push main of griasdi\"]\n",
            ),
        )
        .unwrap();
        assert_eq!(
            config.repos["scan-rs"].url,
            "https://git.example/o/scan-rs.git"
        );
        assert_eq!(config.repos.len(), 3);
    }

    #[test]
    fn an_onboarded_repository_is_refused_when_it_repeats_one() {
        let err = CiConfig::from_parts(EXAMPLE, &drop_in("smoke", "url = \"/elsewhere.git\""))
            .unwrap_err();
        assert_eq!(
            err,
            "ci/onboarded/smoke.toml: ci.toml defines repository smoke already"
        );
        let base = format!(
            "{}[repos.r]\nurl = \"git@git.example:O/R.git\"\n",
            with_runners("")
        );
        let err = CiConfig::from_parts(
            &base,
            &drop_in("other", "url = \"https://git.example/o/r\""),
        )
        .unwrap_err();
        assert_eq!(err, "ci/onboarded/other.toml: its url is repository r's");
        let err = CiConfig::from_parts(&base, &drop_in("Bad", "url = \"x\"")).unwrap_err();
        assert_eq!(
            err,
            "ci/onboarded/Bad.toml: the file name is no repository name"
        );
    }

    #[test]
    fn an_onboarded_grant_must_parse() {
        let err = CiConfig::from_parts(
            EXAMPLE,
            &drop_in(
                "new",
                "url = \"x\"\n[grants]\nGITHUB_TOKEN = [\"cron *\"]\n",
            ),
        )
        .unwrap_err();
        assert!(
            err.starts_with("repo `new`: grant for GITHUB_TOKEN: "),
            "{err}"
        );
    }

    #[test]
    fn a_reload_swaps_repositories_and_refuses_anything_else() {
        let shared = Shared::new(CiConfig::parse(EXAMPLE).unwrap());
        let next = CiConfig::from_parts(EXAMPLE, &drop_in("new", "url = \"/new.git\"")).unwrap();
        assert_eq!(shared.replace(next).unwrap(), ["new"]);
        assert!(shared.get().repos.contains_key("new"));
        assert_eq!(
            shared.replace(CiConfig::parse(EXAMPLE).unwrap()).unwrap(),
            ["new"]
        );
        assert!(
            shared
                .replace(CiConfig::parse(EXAMPLE).unwrap())
                .unwrap()
                .is_empty()
        );
        let runners = EXAMPLE.replace("wait_minutes = 30", "wait_minutes = 31");
        assert_eq!(
            shared
                .replace(CiConfig::parse(&runners).unwrap())
                .unwrap_err(),
            "a change outside [repos] takes a restart"
        );
        assert_eq!(shared.get().runners["atik"].wait_minutes, 30);
    }

    #[test]
    fn onboarding_needs_its_fields_and_secrets() {
        let table = "[onboarding]\napi = \"https://git.example/\"\nbot = \"ci\"\nbot_token = \"BOT\"\nadmin_token = \"ADMIN\"\nhook_url = \"https://ci.example/hook/\"\n";
        let config = CiConfig::parse(&format!("{EXAMPLE}{table}")).unwrap();
        let onboarding = config.onboarding.unwrap();
        assert_eq!(onboarding.api, "https://git.example");
        assert_eq!(onboarding.hook_url, "https://ci.example/hook");
        let err = CiConfig::parse(&format!("{}{table}", with_runners(""))).unwrap_err();
        assert_eq!(
            err,
            "[onboarding] names secrets but ci.toml has no [secrets]"
        );
        let err = CiConfig::parse(&format!("{EXAMPLE}{}", table.replace("\"ci\"", "\"a b\"")))
            .unwrap_err();
        assert!(err.contains("`bot`"), "{err}");
        let err = CiConfig::parse(&format!("{EXAMPLE}{}", table.replace("\"BOT\"", "\"bot\"")))
            .unwrap_err();
        assert!(err.contains("`bot_token`"), "{err}");
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;
        use std::path::PathBuf;

        fn temp(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("zci-config-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn load_missing_is_none() {
            let dir = temp("load");
            assert!(CiConfig::load(&dir).unwrap().is_none());
            std::fs::write(dir.join(CONFIG_FILE), with_runners("")).unwrap();
            assert_eq!(CiConfig::load(&dir).unwrap().unwrap().default_runner, "a");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
