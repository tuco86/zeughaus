//! `<state-dir>/ci.toml`.
//!
//! The config only ever names secrets; their values live in OpenBao
//! (`[secrets]`, read by `super::secrets`), so `ci.toml` holds nothing that
//! must stay private.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::event::CiEvent;
use super::pipeline::Pattern;

pub const CONFIG_FILE: &str = "ci.toml";

fn default_budget_gb() -> u64 {
    50
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiConfig {
    /// Size limit of the host workspaces under `ci/work`.
    #[serde(default = "default_budget_gb")]
    pub budget_gb: u64,
    #[serde(default)]
    pub busy: BusyConfig,
    #[serde(default)]
    pub containers: ContainerConfig,
    #[serde(default)]
    pub machines: BTreeMap<String, MachineConfig>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BusyConfig {
    /// `nvidia-smi` utilization.gpu threshold in percent.
    pub gpu_percent: u32,
    /// Mean over this window at or above the threshold means busy.
    pub busy_after_seconds: u64,
    /// Mean over this window below the threshold means free.
    pub free_after_seconds: u64,
}

impl Default for BusyConfig {
    fn default() -> Self {
        Self {
            gpu_percent: 50,
            busy_after_seconds: 60,
            free_after_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ContainerConfig {
    pub cpus: u32,
    pub memory: String,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            cpus: 12,
            memory: "24g".into(),
        }
    }
}

fn default_machine_cpus() -> u32 {
    8
}

fn default_machine_memory() -> String {
    "12G".into()
}

fn default_cache_disk() -> String {
    "100G".into()
}

fn default_idle_minutes() -> u64 {
    20
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub script: PathBuf,
    pub dir: PathBuf,
    pub ssh_host: String,
    #[serde(default = "default_machine_cpus")]
    pub cpus: u32,
    #[serde(default = "default_machine_memory")]
    pub memory: String,
    #[serde(default = "default_cache_disk")]
    pub cache_disk: String,
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u64,
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
        for name in self.machines.keys() {
            if !valid_name(name) {
                return Err(format!(
                    "machine name `{name}` must match [a-z0-9][a-z0-9-]*"
                ));
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
    use crate::ci::event::EventKind;

    const EXAMPLE: &str = r#"
budget_gb = 50
[busy]
gpu_percent = 50
busy_after_seconds = 60
free_after_seconds = 300
[containers]
cpus = 12
memory = "24g"
[machines.win11]
script = "/usr/local/lib/zeughaus-ci/vm/win11/vm.sh"
dir = "/var/lib/zeughaus-ci/vm/win11"
ssh_host = "win11-ci"
cpus = 8
memory = "12G"
cache_disk = "100G"
idle_minutes = 20
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
        assert_eq!(config.budget_gb, 50);
        assert_eq!(config.busy.free_after_seconds, 300);
        assert_eq!(config.containers.memory, "24g");
        let win = &config.machines["win11"];
        assert_eq!(win.ssh_host, "win11-ci");
        assert_eq!(win.idle_minutes, 20);
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
    fn empty_config_has_defaults() {
        let config = CiConfig::parse("").unwrap();
        assert_eq!(config.budget_gb, 50);
        assert_eq!(config.busy.gpu_percent, 50);
        assert_eq!(config.containers.cpus, 12);
        assert!(config.repos.is_empty());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(CiConfig::parse("bogus = 1").is_err());
        assert!(CiConfig::parse("[busy]\nnope = 1").is_err());
        assert!(CiConfig::parse("[repos.a]\nurl = \"x\"\nnope = 1").is_err());
    }

    #[test]
    fn validation() {
        assert!(CiConfig::parse("[repos.Bad]\nurl = \"x\"").is_err());
        assert!(CiConfig::parse("[repos.a]\nurl = \"x\"\nfetch_token = \"lower\"").is_err());
        assert!(CiConfig::parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nlower = []").is_err());
        assert!(
            CiConfig::parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nA = [\"cron 1 2\"]")
                .is_err()
        );
        assert!(CiConfig::parse("[repos.a]\nurl = \"x\"\nforge = \"github\"").is_err());
        assert!(
            CiConfig::parse("[machines.W]\nscript = \"s\"\ndir = \"d\"\nssh_host = \"h\"").is_err()
        );
        assert!(CiConfig::parse("[machines.w]\nscript = \"s\"\ndir = \"d\"").is_err());
    }

    #[test]
    fn secrets_need_a_source() {
        let err = CiConfig::parse("[repos.a]\nurl = \"x\"\nfetch_token = \"TOKEN\"").unwrap_err();
        assert_eq!(err, "repo `a` names secrets but ci.toml has no [secrets]");
        let err =
            CiConfig::parse("[repos.a]\nurl = \"x\"\n[repos.a.grants]\nTOKEN = []").unwrap_err();
        assert_eq!(err, "repo `a` names secrets but ci.toml has no [secrets]");
        assert!(CiConfig::parse("[secrets]\naddr = \"a\"\npath = \"p\"\nrole_id = \"\"").is_err());
        let config =
            CiConfig::parse("[secrets]\naddr = \"http://b:8200/\"\npath = \"p\"\nrole_id = \"r\"")
                .unwrap();
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
        std::fs::write(dir.join(CONFIG_FILE), "budget_gb = 7").unwrap();
        assert_eq!(CiConfig::load(&dir).unwrap().unwrap().budget_gb, 7);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
