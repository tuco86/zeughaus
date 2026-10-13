//! Disk housekeeping after a run: the workspace budget, container images
//! nothing uses any more, and artifact sets of old pipelines.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use zeughaus_link::enlist::parse_run_id;

/// Pipelines whose artifacts are kept per repository.
pub const KEEP_ARTIFACTS: usize = 10;

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
            eprintln!("[enlist] removed image {tag}");
        }
    }
    let _ = Command::new("podman")
        .args(["image", "prune", "-f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Keeps the artifact sets (`ci/artifacts/<run>`) of the newest `keep`
/// pipelines of every repository, and always those of the runs in `live`.
/// A directory that is not a run id is not ours and stays.
pub fn prune_artifacts(state_dir: &Path, keep: usize, live: &HashSet<String>) {
    let root = super::artifacts_dir(state_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut by_repo: BTreeMap<String, Vec<(u64, PathBuf, String)>> = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((repo, number, _)) = parse_run_id(&name) {
            by_repo
                .entry(repo)
                .or_default()
                .push((number, entry.path(), name));
        }
    }
    for runs in by_repo.values() {
        let mut numbers: Vec<u64> = runs.iter().map(|(number, _, _)| *number).collect();
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        numbers.dedup();
        let Some(&oldest_kept) = numbers.get(keep.saturating_sub(1)) else {
            continue;
        };
        for (number, path, run) in runs {
            if *number >= oldest_kept || live.contains(run) {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(path) {
                eprintln!("[enlist] cannot remove {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_pipelines_keep_their_artifacts() {
        let state = std::env::temp_dir().join(format!("zci-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let root = crate::enlist::artifacts_dir(&state);
        for number in 1..=4u64 {
            std::fs::create_dir_all(root.join(format!("demo.{number}.build"))).unwrap();
        }
        std::fs::create_dir_all(root.join("other.1.build")).unwrap();
        std::fs::create_dir_all(root.join("not-a-run")).unwrap();
        let live = HashSet::from(["demo.1.build".to_owned()]);
        prune_artifacts(&state, 2, &live);
        let mut left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let _ = std::fs::remove_dir_all(&state);
        assert_eq!(
            left,
            [
                "demo.1.build",
                "demo.3.build",
                "demo.4.build",
                "not-a-run",
                "other.1.build"
            ]
        );
    }
}
