//! Disk housekeeping after a run: the workspace budget, container images
//! superseded or unused for two weeks, and the removal of trees a container
//! job may have left files in.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use super::stage::Engine;

/// The workspaces under `ci/work/<repo>/<workspace>`.
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

/// When a workspace was last checked out: the marker every checkout touches
/// in the workspace's `.git`. A workspace without it counts as the oldest.
fn last_used(workspace: &Path) -> SystemTime {
    std::fs::metadata(workspace.join(".git").join("zci-last-used"))
        .and_then(|marker| marker.modified())
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

/// The name prefix of every image the CI's container jobs build
/// (`localhost/zci-<repo>-<image>:<digest>`, set in zeughaus-ci's scheduler).
const IMAGE_PREFIX: &str = "localhost/zci-";

/// How long an image whose name no live run uses is kept after its last use.
const IMAGE_IDLE_SECS: u64 = 14 * 24 * 60 * 60;

/// The image name of a tag: everything before its last `:`.
fn image_name(tag: &str) -> &str {
    tag.rsplit_once(':').map_or(tag, |(name, _)| name)
}

/// Which of the CI images in `listed` (the engine's image list, one
/// `name:tag` per line) to remove, given the tags of the live runs in
/// `used`: a tag superseded by a used tag of the same name, and a tag
/// whose name no run uses and that was last used `IMAGE_IDLE_SECS` or
/// more before `now`. Updates `last_used` (tag -> unix seconds): used
/// tags are stamped `now`, a listed tag it has no entry for gets `now`,
/// and entries of tags that are no longer listed are dropped.
fn stale_images(
    listed: &str,
    used: &HashSet<String>,
    last_used: &mut BTreeMap<String, u64>,
    now: u64,
) -> Vec<String> {
    let ours: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|tag| tag.starts_with(IMAGE_PREFIX))
        .collect();
    for tag in used {
        last_used.insert(tag.clone(), now);
    }
    last_used.retain(|tag, _| used.contains(tag) || ours.contains(&tag.as_str()));
    let used_names: HashSet<&str> = used.iter().map(|tag| image_name(tag)).collect();
    ours.into_iter()
        .filter(|tag| !used.contains(*tag))
        .filter(|tag| {
            if used_names.contains(image_name(tag)) {
                return true;
            }
            let seen = *last_used.entry((*tag).to_owned()).or_insert(now);
            now.saturating_sub(seen) >= IMAGE_IDLE_SECS
        })
        .map(str::to_owned)
        .collect()
}

/// Removes the CI image tags `stale_images` picks against `used` (the tags
/// of every live run of this runner), keeping each tag's last use in
/// `record`, then the layers nothing references any more. A missing
/// container engine means there is nothing to prune.
pub fn prune_images(used: &HashSet<String>, record: &Path) {
    let engine = Engine::host();
    let program = engine.program();
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
    // A missing or unreadable record is an empty one.
    let mut last_used: BTreeMap<String, u64> = crate::files::read_json(record).unwrap_or_default();
    let stale = stale_images(
        &String::from_utf8_lossy(&output.stdout),
        used,
        &mut last_used,
        super::now_secs(),
    );
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
    if let Err(e) = crate::files::write_json(record, &last_used) {
        eprintln!("[enlist] images: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000;

    fn tags(list: &[&str]) -> HashSet<String> {
        list.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn a_tag_is_removed_once_a_newer_tag_of_its_image_is_used() {
        let listed = "localhost/zci-weida-trixie:old\nlocalhost/zci-weida-trixie:new\n";
        let used = tags(&["localhost/zci-weida-trixie:new"]);
        let mut record = BTreeMap::new();
        assert_eq!(
            stale_images(listed, &used, &mut record, NOW),
            ["localhost/zci-weida-trixie:old"]
        );
    }

    #[test]
    fn an_image_of_another_containerfile_is_kept_while_recently_used() {
        let listed = "localhost/zci-weida-trixie:new\n\
                      localhost/zci-weida-python:x\n\
                      localhost/zci-griasdi-ubuntu24:a\n";
        let used = tags(&["localhost/zci-weida-trixie:new"]);
        let mut record = BTreeMap::new();
        assert!(stale_images(listed, &used, &mut record, NOW).is_empty());
        let expected: BTreeMap<String, u64> = [
            "localhost/zci-weida-trixie:new",
            "localhost/zci-weida-python:x",
            "localhost/zci-griasdi-ubuntu24:a",
        ]
        .into_iter()
        .map(|t| (t.to_owned(), NOW))
        .collect();
        assert_eq!(record, expected);
    }

    #[test]
    fn an_image_unused_for_the_idle_time_is_removed() {
        let listed = "localhost/zci-weida-python:x\n";
        let used = HashSet::new();
        let tag = "localhost/zci-weida-python:x".to_owned();

        let mut record = BTreeMap::from([(tag.clone(), NOW - IMAGE_IDLE_SECS)]);
        assert_eq!(
            stale_images(listed, &used, &mut record, NOW),
            [tag.as_str()]
        );

        let recent = NOW - IMAGE_IDLE_SECS + 1;
        let mut record = BTreeMap::from([(tag.clone(), recent)]);
        assert!(stale_images(listed, &used, &mut record, NOW).is_empty());
        assert_eq!(record.get(&tag), Some(&recent));
    }

    #[test]
    fn the_record_forgets_images_that_are_gone() {
        let listed = "docker.io/library/debian:trixie\n<none>:<none>\n";
        let mut record = BTreeMap::from([("localhost/zci-weida-js:gone".to_owned(), NOW - 10)]);
        assert!(stale_images(listed, &HashSet::new(), &mut record, NOW).is_empty());
        assert!(record.is_empty());
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use std::fs::File;
        use std::time::Duration;

        fn marked(path: &Path, secs: u64) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            File::create(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        }

        /// A workspace is as old as the marker in its `.git`; one without it,
        /// whatever lies beside its `.git`, counts as the oldest.
        #[test]
        fn a_workspace_is_as_old_as_its_marker() {
            let base = std::env::temp_dir().join(format!("zci-last-used-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            marked(&base.join("repo/.git/zci-last-used"), 100);
            marked(&base.join("beside/.zci-last-used"), 200);
            std::fs::create_dir_all(base.join("never")).unwrap();
            let secs = |ws: &str| {
                last_used(&base.join(ws))
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            };
            assert_eq!(secs("repo"), 100);
            assert_eq!(secs("beside"), 0);
            assert_eq!(secs("never"), 0);
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
