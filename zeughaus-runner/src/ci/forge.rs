//! Commit statuses on GitHub and Forgejo, posted through `curl`.
//!
//! The token travels as a header on curl's stdin (`-H @-`), never in argv,
//! where every user on the machine could read it from `/proc`. Posting is
//! best effort: a status that does not arrive is logged and the pipeline
//! carries on, because the forge being down is no reason for a build to fail.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use super::config::{Forge, RepoConfig};
use super::secrets::Secrets;

/// Longest description GitHub accepts.
const MAX_DESCRIPTION: usize = 140;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Success,
    Failure,
    Error,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Success => "success",
            State::Failure => "failure",
            State::Error => "error",
        }
    }
}

/// One status to post.
pub struct Status {
    pub repo_name: String,
    pub repo: RepoConfig,
    pub sha: String,
    /// The job name; the context is `zeughaus/<job>`.
    pub job: String,
    pub state: State,
    pub description: String,
}

/// Posts statuses on a thread of its own, in the order they were sent: a
/// `success` must not overtake the `pending` before it, and the scheduler's
/// tick must not wait twenty seconds for a forge that does not answer.
pub fn poster(state_dir: PathBuf, secrets: Arc<Secrets>) -> Sender<Status> {
    let (tx, rx) = std::sync::mpsc::channel::<Status>();
    let spawned = std::thread::Builder::new()
        .name("zeughaus-ci-status".into())
        .spawn(move || post_all(&state_dir, &secrets, rx));
    if let Err(e) = spawned {
        eprintln!("[ci] no status thread, statuses are not posted: {e}");
    }
    tx
}

fn post_all(state_dir: &Path, secrets: &Secrets, rx: Receiver<Status>) {
    for status in rx {
        if let Err(e) = post(state_dir, secrets, &status) {
            eprintln!("[ci] status {} {}: {e}", status.repo_name, status.job);
        }
    }
}

/// The description cut to what the forge accepts, on a character boundary.
fn truncate(text: &str) -> String {
    text.chars().take(MAX_DESCRIPTION).collect()
}

/// `<text> (#<pipeline>)`, with `text` cut so the pipeline number survives
/// the forge's limit.
pub fn describe(text: &str, pipeline: u64) -> String {
    let suffix = format!(" (#{pipeline})");
    let room = MAX_DESCRIPTION.saturating_sub(suffix.chars().count());
    if text.chars().count() <= room {
        return format!("{text}{suffix}");
    }
    let cut: String = text.chars().take(room.saturating_sub(3)).collect();
    format!("{}...{suffix}", cut.trim_end())
}

/// The authorization header for `repo`'s forge.
fn auth_header(forge: Forge, token: &str) -> String {
    match forge {
        Forge::Github => {
            format!("Authorization: Bearer {token}\nAccept: application/vnd.github+json\n")
        }
        Forge::Forgejo => format!("Authorization: token {token}\n"),
        Forge::None => String::new(),
    }
}

fn statuses_url(repo: &RepoConfig, sha: &str) -> Option<String> {
    match repo.forge {
        Forge::Github => Some(format!("{}/repos/{}/statuses/{sha}", repo.api, repo.slug)),
        Forge::Forgejo => Some(format!(
            "{}/api/v1/repos/{}/statuses/{sha}",
            repo.api, repo.slug
        )),
        Forge::None => None,
    }
}

fn repo_url(repo: &RepoConfig) -> Option<String> {
    match repo.forge {
        Forge::Github => Some(format!("{}/repos/{}", repo.api, repo.slug)),
        Forge::Forgejo => Some(format!("{}/api/v1/repos/{}", repo.api, repo.slug)),
        Forge::None => None,
    }
}

fn post(state_dir: &Path, secrets: &Secrets, status: &Status) -> Result<(), String> {
    let Some(url) = statuses_url(&status.repo, &status.sha) else {
        return Ok(());
    };
    let Some(secret) = &status.repo.status_token else {
        return Ok(());
    };
    let token = secrets.get(secret).map_err(|e| e.to_string())?;
    let body = serde_json::json!({
        "state": status.state.as_str(),
        "context": format!("zeughaus/{}", status.job),
        "description": truncate(&status.description),
    });
    let body_file = state_dir.join("ci").join("tmp").join(format!(
        "status-{}-{}.json",
        std::process::id(),
        status.job
    ));
    crate::files::write_atomic(&body_file, body.to_string().as_bytes())?;
    let result = curl(
        &["-X", "POST", "-H", "Content-Type: application/json"],
        &auth_header(status.repo.forge, &token),
        Some(&body_file),
        &url,
    );
    let _ = std::fs::remove_file(&body_file);
    result.map(|_| ())
}

/// Runs curl with `headers` on stdin; returns the response body.
fn curl(args: &[&str], headers: &str, body: Option<&Path>, url: &str) -> Result<String, String> {
    let mut command = Command::new("curl");
    command.args(["-sS", "--fail-with-body", "--max-time", "20", "-H", "@-"]);
    command.args(args);
    if let Some(body) = body {
        let mut data = std::ffi::OsString::from("@");
        data.push(body);
        command.arg("--data-binary").arg(data);
    }
    command
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(headers.as_bytes())
            .map_err(|e| format!("cannot hand curl its headers: {e}"))?;
    }
    let output = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut message = format!("{} {}", stderr.trim(), stdout.trim());
    // curl reports the HTTP code in its error line with --fail-with-body.
    if stderr.contains("error: 401") {
        message.push_str(" (token expired or revoked?)");
    }
    Err(message.trim().to_owned())
}

/// `ci forge-check`: whether the status token can see the repository.
pub fn check(secrets: &Secrets, repo: &RepoConfig) -> Result<String, String> {
    let Some(url) = repo_url(repo) else {
        return Err("the repo has forge = \"none\"".to_owned());
    };
    let Some(secret) = &repo.status_token else {
        return Err("the repo has no status_token".to_owned());
    };
    let token = secrets.get(secret).map_err(|e| e.to_string())?;
    let body = curl(&[], &auth_header(repo.forge, &token), None, &url)?;
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("unexpected answer: {e}"))?;
    let name = value
        .get("full_name")
        .and_then(|v| v.as_str())
        .ok_or("the answer names no repository")?;
    Ok(format!("ok {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_are_cut_on_a_character_boundary() {
        let long = "ä".repeat(200);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), MAX_DESCRIPTION);
        assert_eq!(truncate("short"), "short");
    }
}
