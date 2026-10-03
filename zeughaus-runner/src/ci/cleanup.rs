//! Disk housekeeping after a pipeline: the workspace budget, container
//! images nothing uses any more, and old pipeline records.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// Pipeline records kept per repository.
const KEEP_PIPELINES: usize = 200;
/// Pipelines whose artifacts are kept per repository.
const KEEP_ARTIFACTS: usize = 10;

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
        match remove_workspace(&ws) {
            Ok(()) => {
                eprintln!("[ci] budget: removed {} ({} MiB)", ws.display(), size >> 20);
                total = total.saturating_sub(size);
            }
            Err(e) => eprintln!("[ci] budget: cannot remove {}: {e}", ws.display()),
        }
    }
}

/// Removes a workspace. A container job leaves files owned by subordinate
/// uids, which only the user namespace podman runs in may delete.
fn remove_workspace(ws: &Path) -> Result<(), String> {
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
/// layers nothing references any more. A missing podman means there is
/// nothing to prune.
pub fn prune_images(repo: &str, used: &HashSet<String>) {
    let prefix = format!("localhost/zci-{repo}-");
    let Ok(output) = Command::new("podman")
        .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
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
        let removed = Command::new("podman")
            .args(["rmi", tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if removed.is_ok_and(|s| s.success()) {
            eprintln!("[ci] removed image {tag}");
        }
    }
    let _ = Command::new("podman")
        .args(["image", "prune", "-f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Keeps the newest pipeline records and artifact directories of `repo`,
/// and always those of the pipelines in `running`.
pub fn prune_pipelines(state_dir: &Path, repo: &str, running: &HashSet<u64>) {
    let dir = super::ci_dir(state_dir).join("pipelines").join(repo);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut records = Vec::new();
    let mut artifacts = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(n) = name
            .strip_suffix(".json")
            .and_then(|n| n.parse::<u64>().ok())
        {
            records.push((n, entry.path()));
        } else if let Some(n) = name
            .strip_suffix(".artifacts")
            .and_then(|n| n.parse::<u64>().ok())
        {
            artifacts.push((n, entry.path()));
        }
    }
    records.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    artifacts.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    for (_, path) in records
        .into_iter()
        .skip(KEEP_PIPELINES)
        .filter(|(n, _)| !running.contains(n))
    {
        let _ = std::fs::remove_file(path);
    }
    for (_, path) in artifacts
        .into_iter()
        .skip(KEEP_ARTIFACTS)
        .filter(|(n, _)| !running.contains(n))
    {
        let _ = std::fs::remove_dir_all(path);
    }
}
