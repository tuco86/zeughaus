//! What the runner can find out about one project for the CI's advice:
//! its listing, its work checkout at `HEAD` (top-level entries, CI
//! folders, manifests and README, remotes, the last commit), and what the
//! forges of `[projects]` say about it. Markdown, cut to
//! [`MAX_FACTS_BYTES`]. Nothing here fails: what cannot be read is said.

use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;
use zeughaus_link::{MAX_FACTS_BYTES, ProjectsReply, normalize_origin};

use super::{Forge, ForgeKind, ProjectsConfig, fetch, token};

/// The most of one file the facts quote.
const EXCERPT_BYTES: usize = 3 << 10;

/// The most top-level entries the facts name.
const MAX_ENTRIES: usize = 80;

/// Files whose head says what a project is and builds, in the order quoted.
const MANIFESTS: [&str; 15] = [
    "README.md",
    "README",
    "README.rst",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "flake.nix",
    "config.toml",
    "zola.toml",
    "Containerfile",
    "Dockerfile",
    "Makefile",
    "justfile",
    "AGENTS.md",
];

/// Folders whose listing says which CI a project has.
const CI_DIRS: [&str; 5] = [
    ".zeughaus-ci",
    ".github/workflows",
    ".forgejo/workflows",
    ".gitea/workflows",
    ".woodpecker",
];

/// The facts about `origin`: the listing of `listed` (the runner's cached
/// listing, if it has one), the checkout under `work_root`, and its forge
/// when there is no checkout.
pub fn gather(
    origin: &str,
    listed: Option<&ProjectsReply>,
    config: &ProjectsConfig,
    work_root: Option<&Path>,
) -> String {
    let origin = normalize_origin(origin);
    let mut out = format!("# {origin}\n\n");
    match listed.and_then(|reply| reply.projects.iter().find(|p| p.origin == origin)) {
        Some(project) => out.push_str(&format!(
            "- listed by the user's forges as `{}` on {}, default branch `{}`\n",
            project.name, project.forge, project.default_branch
        )),
        None => out.push_str("- not in the listing of the user's forges\n"),
    }
    let checkout = work_root
        .map(|root| root.join(&origin))
        .filter(|dir| dir.join(".git").exists());
    match checkout {
        Some(dir) => checkout_facts(&dir, &mut out),
        None => {
            out.push_str("- no work checkout on this machine\n");
            forge_facts(&origin, config, &mut out);
        }
    }
    cut(&mut out, MAX_FACTS_BYTES);
    out
}

/// The work checkout at `HEAD`.
fn checkout_facts(dir: &Path, out: &mut String) {
    out.push_str("\n## Work checkout\n\n");
    if let Some(branch) = git(dir, &["branch", "--show-current"]) {
        out.push_str(&format!("- branch: `{}`\n", branch.trim()));
    }
    if let Some(commit) = git(dir, &["log", "-1", "--format=%cs %s"]) {
        out.push_str(&format!("- last commit: {}\n", commit.trim()));
    }
    if let Some(remotes) = git(dir, &["remote", "-v"]) {
        for line in remotes.lines().filter(|l| l.ends_with("(fetch)")) {
            let mut parts = line.split_whitespace();
            if let (Some(name), Some(url)) = (parts.next(), parts.next()) {
                out.push_str(&format!("- remote `{name}`: {url}\n"));
            }
        }
    }
    let Some(tree) = git(dir, &["ls-tree", "HEAD"]) else {
        out.push_str("- no commit at HEAD\n");
        return;
    };
    let entries = top_level(&tree);
    out.push_str(&format!("\nTop level at HEAD: {}\n", entries.join(", ")));
    for ci in CI_DIRS {
        if let Some(listing) = git(dir, &["ls-tree", "--name-only", &format!("HEAD:{ci}")]) {
            let names: Vec<&str> = listing.lines().take(MAX_ENTRIES).collect();
            out.push_str(&format!("\n`{ci}/`: {}\n", names.join(", ")));
        }
    }
    for file in MANIFESTS {
        if !entries.iter().any(|entry| entry == file) {
            continue;
        }
        if let Some(text) = git(dir, &["show", &format!("HEAD:{file}")]) {
            quote(file, &text, out);
        }
    }
}

/// `git ls-tree HEAD` as entry names, a folder with a trailing `/`.
fn top_level(tree: &str) -> Vec<String> {
    tree.lines()
        .filter_map(|line| {
            let (meta, name) = line.split_once('\t')?;
            Some(match meta.split_whitespace().nth(1) {
                Some("tree") => format!("{name}/"),
                _ => name.to_owned(),
            })
        })
        .take(MAX_ENTRIES)
        .collect()
}

/// What the forge of `origin` says, when `[projects]` names it.
fn forge_facts(origin: &str, config: &ProjectsConfig, out: &mut String) {
    let Some((host, slug)) = origin.split_once('/') else {
        return;
    };
    let forge = config.forges.iter().find(|forge| match forge.kind {
        ForgeKind::Github => host == "github.com",
        ForgeKind::Forgejo => normalize_origin(&forge.url) == host,
    });
    let Some(forge) = forge else {
        out.push_str(&format!("- no forge of `[projects]` serves {host}\n"));
        return;
    };
    let key = match token(&forge.token) {
        Ok(key) => key,
        Err(e) => {
            out.push_str(&format!("- the forge's token: {e}\n"));
            return;
        }
    };
    let (api, headers) = api_of(forge, &key);
    out.push_str(&format!("\n## {} ({})\n\n", host, forge.kind.name()));
    match fetch(&format!("{api}/repos/{slug}"), &headers) {
        Ok(json) => out.push_str(&repo_lines(&json)),
        Err(e) => {
            out.push_str(&format!("- the repository: {e}\n"));
            return;
        }
    }
    if forge.kind == ForgeKind::Forgejo
        && let Ok(json) = fetch(&format!("{api}/repos/{slug}/push_mirrors"), &headers)
    {
        for mirror in mirror_addresses(&json) {
            out.push_str(&format!("- push mirror to {mirror}\n"));
        }
    }
    if let Ok(json) = fetch(&format!("{api}/repos/{slug}/contents"), &headers) {
        let names = content_names(&json);
        out.push_str(&format!("\nTop level: {}\n", names.join(", ")));
    }
    let readme = match forge.kind {
        ForgeKind::Github => fetch(
            &format!("{api}/repos/{slug}/readme"),
            &format!("{headers}Accept: application/vnd.github.raw\n"),
        ),
        ForgeKind::Forgejo => fetch(&format!("{api}/repos/{slug}/raw/README.md"), &headers),
    };
    if let Ok(text) = readme {
        quote("README", &text, out);
    }
    // A repository on GitHub may already have a copy on a Forgejo.
    if forge.kind == ForgeKind::Github
        && let Some(name) = slug.rsplit('/').next()
    {
        for other in config
            .forges
            .iter()
            .filter(|f| f.kind == ForgeKind::Forgejo)
        {
            let Ok(key) = token(&other.token) else {
                continue;
            };
            let (api, headers) = api_of(other, &key);
            if let Ok(json) = fetch(&format!("{api}/repos/search?q={name}&limit=10"), &headers) {
                for found in search_hits(&json, name) {
                    out.push_str(&format!(
                        "- a repository of that name on {}: {found}\n",
                        normalize_origin(&other.url)
                    ));
                }
            }
        }
    }
}

/// The API base of `forge` and its request headers, one per line.
fn api_of(forge: &Forge, token: &str) -> (String, String) {
    match forge.kind {
        ForgeKind::Forgejo => (
            format!("{}/api/v1", forge.url),
            format!("Authorization: token {token}\n"),
        ),
        ForgeKind::Github => (
            "https://api.github.com".to_owned(),
            format!("Authorization: Bearer {token}\n"),
        ),
    }
}

/// A repository's description, language, flags and dates, from the
/// `repos/<slug>` answer of Forgejo or GitHub.
fn repo_lines(json: &str) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Repo {
        description: Option<String>,
        language: Option<String>,
        default_branch: Option<String>,
        private: bool,
        fork: bool,
        archived: bool,
        /// Forgejo: a pull mirror.
        mirror: bool,
        /// Forgejo: where a pull mirror pulls from.
        original_url: Option<String>,
        /// GitHub: where a mirror pulls from.
        mirror_url: Option<String>,
        topics: Vec<String>,
        updated_at: Option<String>,
    }
    let Ok(repo) = serde_json::from_str::<Repo>(json) else {
        return "- the forge answered something unreadable\n".to_owned();
    };
    let mut lines = String::new();
    let mut line = |text: String| {
        lines.push_str("- ");
        lines.push_str(&text);
        lines.push('\n');
    };
    if let Some(description) = repo.description.filter(|d| !d.is_empty()) {
        line(format!("description: {description}"));
    }
    if let Some(language) = repo.language.filter(|l| !l.is_empty()) {
        line(format!("language: {language}"));
    }
    if let Some(branch) = repo.default_branch {
        line(format!("default branch: `{branch}`"));
    }
    let flags: Vec<&str> = [
        (repo.private, "private"),
        (repo.fork, "a fork"),
        (repo.archived, "archived"),
        (repo.mirror, "a pull mirror"),
    ]
    .into_iter()
    .filter_map(|(set, word)| set.then_some(word))
    .collect();
    if !flags.is_empty() {
        line(flags.join(", "));
    }
    if let Some(from) = repo
        .original_url
        .or(repo.mirror_url)
        .filter(|u| !u.is_empty())
    {
        line(format!("mirrors {from}"));
    }
    if !repo.topics.is_empty() {
        line(format!("topics: {}", repo.topics.join(", ")));
    }
    if let Some(updated) = repo.updated_at {
        line(format!("updated: {updated}"));
    }
    lines
}

/// The remote addresses of Forgejo's `push_mirrors` answer.
fn mirror_addresses(json: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Mirror {
        remote_address: String,
    }
    serde_json::from_str::<Vec<Mirror>>(json)
        .map(|mirrors| mirrors.into_iter().map(|m| m.remote_address).collect())
        .unwrap_or_default()
}

/// The names of a `contents` answer, a folder with a trailing `/`.
fn content_names(json: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        #[serde(rename = "type")]
        kind: String,
    }
    serde_json::from_str::<Vec<Entry>>(json)
        .map(|entries| {
            entries
                .into_iter()
                .take(MAX_ENTRIES)
                .map(|e| match e.kind.as_str() {
                    "dir" => format!("{}/", e.name),
                    _ => e.name,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `full_name`s of Forgejo's `repos/search` answer that are called
/// exactly `name`.
fn search_hits(json: &str, name: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Found {
        data: Vec<Hit>,
    }
    #[derive(Deserialize)]
    struct Hit {
        name: String,
        full_name: String,
    }
    serde_json::from_str::<Found>(json)
        .map(|found| {
            found
                .data
                .into_iter()
                .filter(|hit| hit.name.eq_ignore_ascii_case(name))
                .map(|hit| hit.full_name)
                .collect()
        })
        .unwrap_or_default()
}

/// The head of `file` as a fenced block.
fn quote(file: &str, text: &str, out: &mut String) {
    let mut head = text.to_owned();
    let cut_off = cut(&mut head, EXCERPT_BYTES);
    out.push_str(&format!(
        "\n`{file}`{}:\n\n````\n",
        if cut_off { " (head)" } else { "" }
    ));
    out.push_str(head.trim_end());
    out.push_str("\n````\n");
}

/// Cuts `text` to at most `limit` bytes on a character boundary; whether
/// it cut.
fn cut(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// `git -C <dir> <args>`'s stdout when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_mirror_on_forgejo_says_where_it_pulls_from() {
        let json = r#"{"description":"Scanner","language":"Rust","default_branch":"main",
            "private":true,"mirror":true,"original_url":"https://github.com/o/scan",
            "topics":[],"updated_at":"2026-10-01T00:00:00Z"}"#;
        assert_eq!(
            repo_lines(json),
            "- description: Scanner\n- language: Rust\n- default branch: `main`\n\
             - private, a pull mirror\n- mirrors https://github.com/o/scan\n\
             - updated: 2026-10-01T00:00:00Z\n"
        );
    }

    #[test]
    fn a_cut_keeps_whole_characters() {
        let mut text = "aä".repeat(4);
        assert!(cut(&mut text, 4));
        assert_eq!(text, "aäa");
        assert!(!cut(&mut text, 4));
    }
}
