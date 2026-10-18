//! The bare mirror a repository's jobs check out from, and the git commands
//! the executor runs on it.

use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine as _;

/// `git -C <dir> <args>`; stdout on success.
pub fn git(dir: &Path, args: &[&str], env: &[(String, String)]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Creates the mirror if needed and fetches every branch and tag of `url`.
///
/// The token travels as an `Authorization` header in the environment of this
/// one git process: it is in no URL, no config file and no argument list a
/// `ps` could show.
pub fn fetch_mirror(
    mirror: &Path,
    url: &str,
    user: &str,
    token: Option<&str>,
) -> Result<(), String> {
    if mirror.join("HEAD").exists() {
        // The configured URL is the CI's to change.
        git(mirror, &["remote", "set-url", "origin", url], &[])?;
    } else {
        std::fs::create_dir_all(mirror)
            .map_err(|e| format!("cannot create {}: {e}", mirror.display()))?;
        git(mirror, &["init", "--bare", "-q"], &[])?;
        git(mirror, &["remote", "add", "origin", url], &[])?;
        // A job's commit may be one no branch points at any more.
        git(
            mirror,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
            &[],
        )?;
    }
    let mut env = Vec::new();
    if let Some(token) = token {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
        env.push(("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()));
        env.push(("GIT_CONFIG_KEY_0".to_owned(), "http.extraHeader".to_owned()));
        env.push((
            "GIT_CONFIG_VALUE_0".to_owned(),
            format!("Authorization: Basic {basic}"),
        ));
    }
    git(
        mirror,
        &[
            "fetch",
            "--prune",
            "-q",
            "origin",
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
        ],
        &env,
    )
    .map(|_| ())
    .map_err(|e| {
        if token.is_some()
            && ["could not read Username", " 401", " 403"]
                .iter()
                .any(|s| e.contains(s))
        {
            format!("{e} (fetch token rejected: expired or revoked?)")
        } else {
            e
        }
    })
}

/// Whether the mirror holds the commit `sha`.
pub fn has_commit(mirror: &Path, sha: &str) -> bool {
    git(
        mirror,
        &["cat-file", "-e", &format!("{sha}^{{commit}}")],
        &[],
    )
    .is_ok()
}

/// Names the commit of pipeline `number` `refs/ci/<number>`, which is what a
/// job's checkout fetches.
pub fn pin_commit(mirror: &Path, number: u64, sha: &str) -> Result<(), String> {
    git(
        mirror,
        &["update-ref", &format!("refs/ci/{number}"), sha],
        &[],
    )
    .map(|_| ())
}

/// Writes `refs/ci/<number>` as a bundle, the form a VM guest fetches from.
pub fn bundle(mirror: &Path, number: u64, to: &Path) -> Result<(), String> {
    git(
        mirror,
        &[
            "bundle",
            "create",
            "-q",
            &to.to_string_lossy(),
            &format!("refs/ci/{number}"),
        ],
        &[],
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        fn run(dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=ci", "-c", "user.email=ci@example.invalid"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        #[test]
        fn a_mirror_fetches_a_commit_and_pins_it_for_a_pipeline() {
            let base = std::env::temp_dir().join(format!("zci-git-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let repo = base.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            run(&repo, &["init", "-q"]);
            std::fs::write(repo.join("a"), "a").unwrap();
            run(&repo, &["add", "."]);
            run(&repo, &["commit", "-q", "-m", "a"]);
            let sha = run(&repo, &["rev-parse", "HEAD"]);

            let mirror = base.join("mirror.git");
            let url = format!("file://{}", repo.display());
            fetch_mirror(&mirror, &url, "x-access-token", None).expect("first fetch");
            // The second one takes the already created mirror.
            fetch_mirror(&mirror, &url, "x-access-token", None).expect("second fetch");
            assert!(has_commit(&mirror, &sha));
            assert!(!has_commit(&mirror, &"0".repeat(40)));
            pin_commit(&mirror, 5, &sha).expect("pin");
            let pinned = git(&mirror, &["rev-parse", "refs/ci/5"], &[]).expect("rev-parse");
            let _ = std::fs::remove_dir_all(&base);
            assert_eq!(pinned.trim(), sha);
        }
    }
}
