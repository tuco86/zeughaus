//! `<state-dir>/ci.toml`.
//!
//! The config only ever names secrets; their values live in OpenBao
//! (`[secrets]`, read by `crate::secrets`), so `ci.toml` holds nothing that
//! must stay private.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::event::CiEvent;
use crate::pipeline::Pattern;

pub const CONFIG_FILE: &str = "ci.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiConfig {
    /// The runner of jobs that name no `machine`.
    pub default_runner: String,
    /// Runners that may enlist, by name.
    #[serde(default)]
    pub runners: BTreeMap<String, RunnerConfig>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoConfig>,
    /// Where the values of the secrets named below live.
    pub secrets: Option<SecretsConfig>,
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub fingerprint: String,
    /// Machines (Windows VMs) this runner boots for jobs with `machine = <name>`.
    #[serde(default)]
    pub machines: Vec<String>,
    /// How long a job waits for this runner to connect before it is skipped.
    #[serde(default = "default_wait_minutes")]
    pub wait_minutes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
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

#[derive(Debug, Clone, Deserialize)]
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
}

impl RepoConfig {
    /// Whether `secret` may be handed to a job run for `event`.
    pub fn grants(&self, secret: &str, event: &CiEvent) -> bool {
        self.grants.get(secret).is_some_and(|patterns| {
            patterns
                .iter()
                .any(|p| Pattern::parse_grant(p).is_ok_and(|p| p.matches(event)))
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

    /// The runner that offers machine `machine`.
    pub fn runner_of_machine(&self, machine: &str) -> Option<&str> {
        self.runners
            .iter()
            .find(|(_, runner)| runner.machines.iter().any(|m| m == machine))
            .map(|(name, _)| name.as_str())
    }

    pub fn parse(text: &str) -> Result<CiConfig, String> {
        let mut config: CiConfig =
            toml::from_str(text).map_err(|e| format!("{CONFIG_FILE}: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    /// `Ok(None)` when there is no `ci.toml`: CI is off.
    pub fn load(state_dir: &Path) -> Result<Option<CiConfig>, String> {
        let path = state_dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Self::parse(&text).map(Some)
    }

    fn validate(&mut self) -> Result<(), String> {
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
            for (secret, patterns) in &repo.grants {
                for pattern in patterns {
                    Pattern::parse_grant(pattern)
                        .map_err(|e| format!("repo `{name}`: grant for {secret}: {e}"))?;
                }
            }
            if repo.forge != Forge::None && (repo.slug.is_empty() || repo.api.is_empty()) {
                return Err(format!("repo `{name}`: a forge needs `slug` and `api`"));
            }
            repo.api = repo.api.trim_end_matches('/').to_string();
        }
        Ok(())
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
    use std::path::PathBuf;

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
GITHUB_TOKEN = ["tag v*", "cron *"]
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
            cron: None,
            actor: "t".into(),
            delivery: None,
            received: 0,
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zci-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn runner_validation() {
        let bad_fp = "default_runner = \"a\"\n[runners.a]\nfingerprint = \"nope\"";
        assert!(CiConfig::parse(bad_fp).is_err());
        let bad_name = format!("default_runner = \"A\"\n[runners.A]\nfingerprint = \"{FP_A}\"");
        assert!(CiConfig::parse(&bad_name).is_err());
        assert!(CiConfig::parse(&with_runners("machines = [\"W\"]")).is_err());
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
        assert!(parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nA = [\"cron 1 2\"]").is_err());
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
        let mut cron = event(EventKind::Cron, "main");
        cron.cron = Some("0 4 * * *".into());
        assert!(repo.grants("GITHUB_TOKEN", &cron));
        assert!(!repo.grants("OTHER", &cron));
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

    #[test]
    fn load_missing_is_none() {
        let dir = temp("load");
        assert!(CiConfig::load(&dir).unwrap().is_none());
        std::fs::write(dir.join(CONFIG_FILE), with_runners("")).unwrap();
        assert_eq!(CiConfig::load(&dir).unwrap().unwrap().default_runner, "a");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
