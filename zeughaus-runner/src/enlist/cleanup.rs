//! Disk housekeeping after a run: the workspace budget, container images
//! nothing uses any more, and the removal of trees a container job may have
//! left files in.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use super::stage::Engine;

/// The workspaces under `ci/work/<repo>/<job>`.
fn workspaces(state_dir: &Path) -> Vec<PathBuf> {
    let work = super::ci_dir(state_dir).join("work");
    let Ok(repos) = std::fs::read_dir(&work) else {
        return Vec::new();
    };
    repos
        .flatten()
        .filter(|repo| repo.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|repo| std::fs::read_dir(repo.path()).ok())
        .flat_map(|jobs| jobs.flatten())
        .filter(|job| job.file_type().is_ok_and(|t| t.is_dir()))
        .map(|job| job.path())
        .collect()
}

/// Bytes of every file under `dir`, symlinks not followed.
fn size_of(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// When a workspace was last checked out; one never touched counts as oldest.
fn last_used(workspace: &Path) -> SystemTime {
    std::fs::metadata(workspace.join(".git").join("zci-last-used"))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Deletes the least recently used workspaces until the rest fit in
/// `budget_gb`. Workspaces in `in_use` are never touched.
pub fn enforce_budget(state_dir: &Path, budget_gb: u64, in_use: &HashSet<PathBuf>) {
    let budget = budget_gb.saturating_mul(1 << 30);
    let mut sized: Vec<(SystemTime, u64, PathBuf)> = workspaces(state_dir)
        .into_iter()
        .map(|ws| (last_used(&ws), size_of(&ws), ws))
        .collect();
    let mut total: u64 = sized.iter().map(|(_, size, _)| size).sum();
    sized.sort_by_key(|(used, _, _)| *used);
    for (_, size, ws) in sized {
        if total <= budget {
            break;
        }
        if in_use.contains(&ws) {
            continue;
        }
        match remove_tree(&ws) {
            Ok(()) => {
                eprintln!(
                    "[enlist] budget: removed {} ({} MiB)",
                    ws.display(),
                    size >> 20
                );
                total = total.saturating_sub(size);
            }
            Err(e) => eprintln!("[enlist] budget: cannot remove {}: {e}", ws.display()),
        }
    }
}

/// Removes a directory tree this runner made. A podman container job leaves
/// files owned by subordinate uids, which only the user namespace podman
/// runs in may delete; Apple's `container` writes host-owned files over
/// virtiofs, so a plain removal does. A missing tree is not an error.
pub fn remove_tree(ws: &Path) -> Result<(), String> {
    if !ws.exists() {
        return Ok(());
    }
    if Engine::host() == Engine::Apple {
        return std::fs::remove_dir_all(ws).map_err(|e| e.to_string());
    }
    let unshare = Command::new("podman")
        .arg("unshare")
        .arg("rm")
        .arg("-rf")
        .arg(ws)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match unshare {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::remove_dir_all(ws).map_err(|e| e.to_string())
        }
        Err(e) => Err(format!("cannot run podman: {e}")),
    }
}

/// Removes this repository's CI image tags other than `used`, then the
/// layers nothing references any more. A missing container engine means
/// there is nothing to prune.
pub fn prune_images(repo: &str, used: &HashSet<String>) {
    let engine = Engine::host();
    let program = engine.program();
    let prefix = format!("localhost/zci-{repo}-");
    let list: &[&str] = match engine {
        Engine::Podman => &["images", "--format", "{{.Repository}}:{{.Tag}}"],
        Engine::Apple => &["image", "list", "--quiet"],
    };
    let Ok(output) = Command::new(program)
        .args(list)
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let stale: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|tag| tag.starts_with(&prefix) && !used.contains(*tag))
        .map(str::to_owned)
        .collect();
    for tag in &stale {
        let remove: Vec<&str> = match engine {
            Engine::Podman => vec!["rmi", tag],
            Engine::Apple => vec!["image", "delete", tag],
        };
        let removed = Command::new(program)
            .args(remove)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if removed.is_ok_and(|s| s.success()) {
            eprintln!("[enlist] removed image {tag}");
        }
    }
    let prune: &[&str] = match engine {
        Engine::Podman => &["image", "prune", "-f"],
        Engine::Apple => &["image", "prune"],
    };
    let _ = Command::new(program)
        .args(prune)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
