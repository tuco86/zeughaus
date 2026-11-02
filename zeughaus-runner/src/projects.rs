//! `/projects`: the repositories this runner's user can open in an editor,
//! and on request the facts about one of them ([`facts`]).
//!
//! Every forge of `[projects]` in `<state-dir>/zeughaus.toml` is asked for
//! the repositories its token may push to; every checkout under the work
//! root is added. A repository pushed from the primary forge to a mirror is
//! listed once, at the primary. The forges are asked again after
//! [`CACHE_FOR`] or when the editor asks for a refresh.

mod facts;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use weida::{Replier, TransferMeta};
use zeughaus_link::{
    MAX_PROJECTS_REQUEST_BYTES, ProjectView, ProjectsReply, ProjectsRequest, normalize_origin,
};

/// How long a listing is answered from the cache.
const CACHE_FOR: Duration = Duration::from_secs(300);

/// The most pages asked of one forge.
const MAX_PAGES: u32 = 20;

/// `[projects]` of `zeughaus.toml`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProjectsConfig {
    /// The forge a mirrored repository is listed at; empty for none.
    pub primary: String,
    pub forges: Vec<Forge>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Forge {
    pub kind: ForgeKind,
    /// The Forgejo base URL; GitHub has its own.
    pub url: String,
    /// The argv whose trimmed stdout is the token.
    pub token: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    Forgejo,
    Github,
}

impl ForgeKind {
    fn name(self) -> &'static str {
        match self {
            ForgeKind::Forgejo => "forgejo",
            ForgeKind::Github => "github",
        }
    }
}

/// Reads `[projects]` of the shared `zeughaus.toml`; every other key is
/// another reader's. A file without the table is an empty configuration.
pub fn parse_config(text: &str) -> Result<ProjectsConfig, String> {
    #[derive(Deserialize)]
    struct File {
        projects: Option<Table>,
    }
    #[derive(Deserialize)]
    struct Table {
        #[serde(default)]
        primary: String,
        #[serde(default)]
        forge: Vec<RawForge>,
    }
    #[derive(Deserialize)]
    struct RawForge {
        kind: String,
        url: Option<String>,
        token: Vec<String>,
    }
    let file: File = toml::from_str(text).map_err(|e| format!("zeughaus.toml: {e}"))?;
    let Some(table) = file.projects else {
        return Ok(ProjectsConfig::default());
    };
    let mut forges = Vec::new();
    for raw in table.forge {
        let kind = match raw.kind.as_str() {
            "forgejo" => ForgeKind::Forgejo,
            "github" => ForgeKind::Github,
            other => return Err(format!("[[projects.forge]]: unknown kind {other:?}")),
        };
        let url = match (kind, raw.url) {
            (ForgeKind::Forgejo, Some(url)) => url.trim_end_matches('/').to_owned(),
            (ForgeKind::Forgejo, None) => {
                return Err("[[projects.forge]]: a forgejo forge needs its url".to_owned());
            }
            (ForgeKind::Github, _) => String::new(),
        };
        if raw.token.is_empty() {
            return Err(format!(
                "[[projects.forge]]: the {} token command is empty",
                kind.name()
            ));
        }
        forges.push(Forge {
            kind,
            url,
            token: raw.token,
        });
    }
    Ok(ProjectsConfig {
        primary: table.primary,
        forges,
    })
}

/// One page of `user/repos` from Forgejo or GitHub, which answer in the
/// same shape: the repositories that are not archived and that the token
/// may push to.
pub fn page(json: &str, forge: ForgeKind) -> Result<Vec<ProjectView>, String> {
    #[derive(Deserialize)]
    struct Repo {
        name: String,
        clone_url: String,
        #[serde(default)]
        default_branch: String,
        #[serde(default)]
        archived: bool,
        #[serde(default)]
        permissions: Permissions,
    }
    #[derive(Deserialize, Default)]
    struct Permissions {
        #[serde(default)]
        push: bool,
    }
    let repos: Vec<Repo> =
        serde_json::from_str(json).map_err(|e| format!("unexpected answer: {e}"))?;
    Ok(repos
        .into_iter()
        .filter(|repo| !repo.archived && repo.permissions.push)
        .map(|repo| ProjectView {
            origin: normalize_origin(&repo.clone_url),
            name: repo.name,
            forge: forge.name().to_owned(),
            default_branch: if repo.default_branch.is_empty() {
                "main".to_owned()
            } else {
                repo.default_branch
            },
            checkout: false,
        })
        .collect())
}

/// The listing as the editor shows it: a mirror of a repository on
/// `primary` dropped, each origin once (the first listing wins), the local
/// checkouts added or marked, sorted by name, then origin.
pub fn merge(listed: Vec<ProjectView>, primary: &str, local: Vec<ProjectView>) -> Vec<ProjectView> {
    let split = |origin: &str| -> (String, String) {
        match origin.split_once('/') {
            Some((host, path)) => (host.to_owned(), path.to_owned()),
            None => (origin.to_owned(), String::new()),
        }
    };
    let on_primary: HashSet<String> = listed
        .iter()
        .map(|p| split(&p.origin))
        .filter(|(host, _)| !primary.is_empty() && host == primary)
        .map(|(_, path)| path)
        .collect();
    let mut seen = HashSet::new();
    let mut merged: Vec<ProjectView> = listed
        .into_iter()
        .filter(|p| {
            let (host, path) = split(&p.origin);
            primary.is_empty() || host == primary || !on_primary.contains(&path)
        })
        .filter(|p| seen.insert(p.origin.clone()))
        .collect();
    for checkout in local {
        match merged.iter_mut().find(|p| p.origin == checkout.origin) {
            Some(listed) => listed.checkout = true,
            None => {
                seen.insert(checkout.origin.clone());
                merged.push(ProjectView {
                    forge: "local".to_owned(),
                    checkout: true,
                    ..checkout
                });
            }
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.origin.cmp(&b.origin)));
    merged
}

/// Runs the token command; its trimmed stdout. An error names the command,
/// never what it printed on stdout.
fn token(argv: &[String]) -> Result<String, String> {
    let program = argv.first().ok_or("empty token command")?;
    let output = Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{program} failed ({})", output.status));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() {
        return Err(format!("{program} printed no token"));
    }
    Ok(token)
}

/// GETs `url` with `headers` (one per line) handed to curl on stdin, so a
/// token never reaches argv.
fn fetch(url: &str, headers: &str) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--fail-with-body",
            "--max-time",
            "20",
            "-H",
            "@-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(headers.as_bytes())
            .map_err(|e| format!("cannot hand curl its headers: {e}"))?;
    }
    let output = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

/// Every repository `forge` lists for its token.
fn list(forge: &Forge) -> Result<Vec<ProjectView>, String> {
    let token = token(&forge.token)?;
    let mut projects = Vec::new();
    for n in 1..=MAX_PAGES {
        let (url, headers, full) = match forge.kind {
            ForgeKind::Forgejo => (
                format!("{}/api/v1/user/repos?limit=50&page={n}", forge.url),
                format!("Authorization: token {token}\n"),
                None,
            ),
            ForgeKind::Github => (
                format!(
                    "https://api.github.com/user/repos?affiliation=owner,collaborator,\
                     organization_member&per_page=100&page={n}"
                ),
                format!("Authorization: Bearer {token}\nAccept: application/vnd.github+json\n"),
                Some(100),
            ),
        };
        let json = fetch(&url, &headers)?;
        let count = serde_json::from_str::<Vec<serde_json::Value>>(&json)
            .map_err(|e| format!("unexpected answer: {e}"))?
            .len();
        projects.extend(page(&json, forge.kind)?);
        let last = match full {
            Some(full) => count < full,
            None => count == 0,
        };
        if last {
            break;
        }
    }
    Ok(projects)
}

/// Every `<root>/<host>/<owner>/<name>` with a `.git` directory, by the
/// origin it fetches from. One whose origin cannot be read is left out.
fn local_checkouts(root: &Path) -> Vec<ProjectView> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    };
    let mut found = Vec::new();
    for host in children(root) {
        for owner in children(&host) {
            for dir in children(&owner) {
                if !dir.join(".git").is_dir() {
                    continue;
                }
                let Ok(output) = Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["remote", "get-url", "origin"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                else {
                    continue;
                };
                if !output.status.success() {
                    continue;
                }
                let origin = normalize_origin(String::from_utf8_lossy(&output.stdout).trim());
                let name = origin.rsplit('/').next().unwrap_or(&origin).to_owned();
                found.push(ProjectView {
                    origin,
                    name,
                    forge: "local".to_owned(),
                    default_branch: "main".to_owned(),
                    checkout: true,
                });
            }
        }
    }
    found
}

/// `[projects]` of `state_dir`'s `zeughaus.toml`; none without the file.
fn read_config(state_dir: &Path) -> Result<ProjectsConfig, String> {
    match std::fs::read_to_string(state_dir.join("zeughaus.toml")) {
        Ok(text) => parse_config(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProjectsConfig::default()),
        Err(e) => Err(format!("cannot read zeughaus.toml: {e}")),
    }
}

/// The whole listing, from the configuration in `state_dir`.
fn discover(state_dir: &Path) -> ProjectsReply {
    let config = match read_config(state_dir) {
        Ok(config) => config,
        Err(e) => {
            return ProjectsReply {
                errors: vec![e],
                ..ProjectsReply::default()
            };
        }
    };
    let mut listed = Vec::new();
    let mut errors = Vec::new();
    for forge in &config.forges {
        match list(forge) {
            Ok(projects) => listed.extend(projects),
            Err(e) => {
                let at = match forge.kind {
                    ForgeKind::Forgejo => forge.url.as_str(),
                    ForgeKind::Github => "github.com",
                };
                errors.push(format!("{} {at}: {e}", forge.kind.name()));
            }
        }
    }
    let local = crate::agents::work_root()
        .map(|root| local_checkouts(&root))
        .unwrap_or_default();
    ProjectsReply {
        projects: merge(listed, &config.primary, local),
        errors,
        facts: None,
    }
}

/// Answers project requests until the replier goes away.
pub async fn serve(replier: Replier, state_dir: PathBuf) {
    let mut cache: Option<(Instant, ProjectsReply)> = None;
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[runner] stopped serving projects: {e}");
                return;
            }
        };
        let asked = match request
            .take_body()
            .collect(MAX_PROJECTS_REQUEST_BYTES)
            .await
        {
            Ok(asked) => asked,
            Err(e) => {
                eprintln!("[runner] unreadable projects request: {e}");
                continue;
            }
        };
        let asked = ProjectsRequest::decode(&asked).unwrap_or_default();
        let answer = match asked.facts {
            // About one project, with the listing as far as it is cached.
            Some(origin) => {
                let (dir, listed) = (state_dir.clone(), cache.as_ref().map(|(_, r)| r.clone()));
                let facts = tokio::task::spawn_blocking(move || {
                    let config = read_config(&dir).unwrap_or_default();
                    facts::gather(
                        &origin,
                        listed.as_ref(),
                        &config,
                        crate::agents::work_root().as_deref(),
                    )
                })
                .await
                .unwrap_or_default();
                ProjectsReply {
                    facts: Some(facts),
                    ..ProjectsReply::default()
                }
            }
            None => {
                let fresh = cache
                    .as_ref()
                    .filter(|(at, _)| !asked.refresh && at.elapsed() < CACHE_FOR)
                    .map(|(_, reply)| reply.clone());
                match fresh {
                    Some(reply) => reply,
                    None => {
                        let dir = state_dir.clone();
                        let reply = tokio::task::spawn_blocking(move || discover(&dir))
                            .await
                            .unwrap_or_default();
                        cache = Some((Instant::now(), reply.clone()));
                        reply
                    }
                }
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[runner] cannot reply to a projects request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&answer.encode()).await {
            eprintln!("[runner] cannot write a projects reply: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[runner] cannot finish a projects reply: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(origin: &str, forge: &str) -> ProjectView {
        ProjectView {
            origin: origin.to_owned(),
            name: origin.rsplit('/').next().unwrap().to_owned(),
            forge: forge.to_owned(),
            default_branch: "main".to_owned(),
            checkout: false,
        }
    }

    const PAGE: &str = r#"[
        {"name":"zeughaus","clone_url":"https://git.example.org/tuco86/zeughaus.git",
         "default_branch":"main","archived":false,"permissions":{"admin":true,"push":true,"pull":true}},
        {"name":"upstream","clone_url":"https://git.example.org/other/upstream.git",
         "default_branch":"master","archived":false,"permissions":{"push":false,"pull":true}},
        {"name":"old","clone_url":"https://git.example.org/tuco86/old.git",
         "default_branch":"main","archived":true,"permissions":{"push":true}}
    ]"#;

    #[test]
    fn a_forgejo_page_keeps_the_pushable_repositories_that_are_not_archived() {
        let projects = page(PAGE, ForgeKind::Forgejo).unwrap();
        assert_eq!(
            projects,
            [view("git.example.org/tuco86/zeughaus", "forgejo")]
        );
    }

    #[test]
    fn a_github_page_keeps_the_pushable_repositories_that_are_not_archived() {
        let page_json = PAGE.replace("git.example.org", "github.com");
        let projects = page(&page_json, ForgeKind::Github).unwrap();
        assert_eq!(projects, [view("github.com/tuco86/zeughaus", "github")]);
    }

    #[test]
    fn a_mirror_of_a_primary_repository_is_listed_once_at_the_primary() {
        let merged = merge(
            vec![
                view("github.com/tuco86/zeughaus", "github"),
                view("git.doodleshnookie.net/tuco86/zeughaus", "forgejo"),
                view("github.com/tuco86/only-here", "github"),
                view("github.com/tuco86/only-here", "github"),
            ],
            "git.doodleshnookie.net",
            Vec::new(),
        );
        let origins: Vec<&str> = merged.iter().map(|p| p.origin.as_str()).collect();
        assert_eq!(
            origins,
            [
                "github.com/tuco86/only-here",
                "git.doodleshnookie.net/tuco86/zeughaus"
            ]
        );
    }

    #[test]
    fn a_local_checkout_is_added_or_marks_its_listing() {
        let merged = merge(
            vec![view("h/o/listed", "forgejo")],
            "",
            vec![view("h/o/listed", "local"), view("h/o/demo", "local")],
        );
        assert_eq!(
            merged,
            [
                ProjectView {
                    checkout: true,
                    ..view("h/o/demo", "local")
                },
                ProjectView {
                    checkout: true,
                    ..view("h/o/listed", "forgejo")
                },
            ]
        );
    }

    #[test]
    fn the_configuration_names_known_forges_and_ignores_other_keys() {
        assert_eq!(
            parse_config("remotes = [\"https://x\"]\n"),
            Ok(ProjectsConfig::default())
        );
        let gitlab = parse_config(
            "[projects]\n[[projects.forge]]\nkind = \"gitlab\"\ntoken = [\"get-secret\"]\n",
        );
        assert!(gitlab.unwrap_err().contains("\"gitlab\""));
        let config = parse_config(
            "ci = []\n[projects]\nprimary = \"h\"\n[[projects.forge]]\nkind = \"forgejo\"\n\
             url = \"https://h/\"\ntoken = [\"get-secret\", \"a/b\"]\n\
             [[projects.forge]]\nkind = \"github\"\ntoken = [\"t\"]\n",
        )
        .unwrap();
        assert_eq!(config.primary, "h");
        assert_eq!(config.forges.len(), 2);
        assert_eq!(config.forges[0].url, "https://h");
        assert!(
            parse_config("[projects]\n[[projects.forge]]\nkind = \"forgejo\"\ntoken = [\"t\"]\n")
                .is_err()
        );
    }
}
