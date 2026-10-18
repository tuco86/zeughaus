//! CI events and the on-disk queue.
//!
//! An event is written to `ci/inbox/` by a webhook or `ci run`, then moved to
//! `ci/queue/<key>.json` by the scheduler. The key is the repository, the
//! event kind and the ref, so a newer event for the same ref replaces the one
//! still waiting; a running pipeline is never cancelled by it. A manual run's
//! key is its upload instead, so no manual run replaces another.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ci_dir;
use crate::files::{read_json, write_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Push,
    Tag,
    Cron,
    /// A working-tree snapshot an editor's agent uploaded on `/manual`, to
    /// run named jobs of the repository's pipeline from.
    Manual,
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EventKind::Push => "push",
            EventKind::Tag => "tag",
            EventKind::Cron => "cron",
            EventKind::Manual => "manual",
        })
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "push" => Ok(EventKind::Push),
            "tag" => Ok(EventKind::Tag),
            "cron" => Ok(EventKind::Cron),
            "manual" => Ok(EventKind::Manual),
            other => Err(format!(
                "unknown event kind `{other}` (push, tag, cron or manual)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiEvent {
    pub repo: String,
    pub kind: EventKind,
    /// Branch or tag name without the `refs/` prefix; the repository's
    /// default branch for a manual run.
    pub git_ref: String,
    pub sha: Option<String>,
    /// The normalized cron expression of a cron event.
    pub cron: Option<String>,
    pub actor: String,
    pub delivery: Option<String>,
    /// The id of the upload a manual run carries, the directory
    /// `ci/manual/<upload>/` holds it; `None` for every other event.
    #[serde(default)]
    pub upload: Option<String>,
    /// Unix seconds.
    pub received: u64,
}

fn inbox_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("inbox")
}

fn queue_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("queue")
}

/// Writes the event as a new inbox file. The name sorts by arrival and the
/// pid keeps two writers in the same nanosecond apart.
pub fn write_inbox(state_dir: &Path, event: &CiEvent) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = inbox_dir(state_dir).join(format!("{nanos:020}-{}.json", std::process::id()));
    write_json(&path, event)?;
    Ok(path)
}

/// `<repo>.<kind>.<first 12 hex of sha256(git_ref \n cron)>`, and for a manual
/// run `<repo>.manual.<upload>`: every manual run runs, none replaces another.
pub fn queue_key(event: &CiEvent) -> String {
    if let (EventKind::Manual, Some(upload)) = (event.kind, &event.upload) {
        return format!("{}.manual.{upload}", event.repo);
    }
    let mut hasher = Sha256::new();
    hasher.update(event.git_ref.as_bytes());
    hasher.update(b"\n");
    hasher.update(event.cron.as_deref().unwrap_or_default().as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{}.{}.{}", event.repo, event.kind, &digest[..12])
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
}

fn quarantine(path: &Path, err: &str) {
    eprintln!("[ci] bad inbox file {}: {err}", path.display());
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    if let Err(e) = std::fs::rename(path, &bad) {
        eprintln!("[ci] cannot move {} aside: {e}", path.display());
    }
}

/// Moves every inbox file into the queue under its key; a file already
/// queued under that key is replaced. A file that does not parse is renamed
/// to `*.bad`.
pub fn drain_inbox(state_dir: &Path) {
    let queue = queue_dir(state_dir);
    for path in json_files(&inbox_dir(state_dir)) {
        match read_json::<CiEvent>(&path) {
            Ok(event) => {
                if let Err(e) = std::fs::create_dir_all(&queue) {
                    eprintln!("[ci] cannot create {}: {e}", queue.display());
                    return;
                }
                let target = queue.join(format!("{}.json", queue_key(&event)));
                if let Err(e) = std::fs::rename(&path, &target) {
                    eprintln!("[ci] cannot queue {}: {e}", path.display());
                }
            }
            Err(e) => quarantine(&path, &e),
        }
    }
}

/// Queued events, oldest first.
pub fn queued(state_dir: &Path) -> Vec<(PathBuf, CiEvent)> {
    let mut out = Vec::new();
    for path in json_files(&queue_dir(state_dir)) {
        match read_json::<CiEvent>(&path) {
            Ok(event) => out.push((path, event)),
            Err(e) => quarantine(&path, &e),
        }
    }
    out.sort_by(|a, b| a.1.received.cmp(&b.1.received).then_with(|| a.0.cmp(&b.0)));
    out
}

fn cancel_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("cancel")
}

/// Asks the scheduler to cancel pipeline `number` of `repo`: an empty file
/// `<repo>.<number>`, which the scheduler removes when it has acted. A
/// repository name holds no `.`, so the name splits unambiguously.
pub fn write_cancel(state_dir: &Path, repo: &str, number: u64) -> Result<(), String> {
    crate::files::write_atomic(&cancel_dir(state_dir).join(format!("{repo}.{number}")), b"")
}

/// Takes every pending cancel request, in name order. A file whose name
/// is not `<repo>.<number>` is removed without a request.
pub fn drain_cancels(state_dir: &Path) -> Vec<(String, u64)> {
    let Ok(entries) = std::fs::read_dir(cancel_dir(state_dir)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // A half-written `.tmp` file is the writer's; it is renamed next.
        if name.ends_with(".tmp") {
            continue;
        }
        if let Some((repo, number)) = name.rsplit_once('.')
            && let Ok(number) = number.parse::<u64>()
        {
            out.push((repo.to_owned(), number));
        }
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("[ci] cannot remove {}: {e}", path.display());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sha: &str, received: u64) -> CiEvent {
        CiEvent {
            repo: "smoke".into(),
            kind: EventKind::Push,
            git_ref: "main".into(),
            sha: Some(sha.into()),
            cron: None,
            actor: "manual".into(),
            delivery: None,
            upload: None,
            received,
        }
    }

    #[test]
    fn key_distinguishes_ref_kind_and_cron() {
        let a = event("x", 0);
        let mut b = a.clone();
        b.git_ref = "dev".into();
        let mut c = a.clone();
        c.kind = EventKind::Tag;
        let mut d = a.clone();
        d.cron = Some("0 4 * * *".into());
        let keys = [a.clone(), b, c, d].map(|e| queue_key(&e));
        for i in 0..keys.len() {
            for j in i + 1..keys.len() {
                assert_ne!(keys[i], keys[j]);
            }
        }
        assert!(queue_key(&a).starts_with("smoke.push."));
        assert_eq!(queue_key(&a).len(), "smoke.push.".len() + 12);
    }

    /// Every manual run is its own key, so none replaces another still
    /// queued; an event that predates the field has no upload.
    #[test]
    fn a_manual_runs_key_is_its_upload() {
        let mut a = event("x", 0);
        a.kind = EventKind::Manual;
        a.upload = Some("0123456789abcdef".into());
        let mut b = a.clone();
        b.upload = Some("fedcba9876543210".into());
        assert_eq!(queue_key(&a), "smoke.manual.0123456789abcdef");
        assert_ne!(queue_key(&a), queue_key(&b));

        let mut old = serde_json::to_value(&a).unwrap();
        assert!(old.as_object_mut().unwrap().remove("upload").is_some());
        let old: CiEvent = serde_json::from_value(old).unwrap();
        assert_eq!(old.upload, None);
    }

    #[test]
    fn kind_round_trips() {
        for kind in [
            EventKind::Push,
            EventKind::Tag,
            EventKind::Cron,
            EventKind::Manual,
        ] {
            assert_eq!(kind.to_string().parse::<EventKind>().unwrap(), kind);
        }
        assert!("pull".parse::<EventKind>().is_err());
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        fn temp(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("zci-event-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn three_events_same_key_leave_the_last() {
            let dir = temp("coalesce");
            for (i, sha) in ["a", "b", "c"].iter().enumerate() {
                write_inbox(&dir, &event(sha, i as u64)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            drain_inbox(&dir);
            let queued = queued(&dir);
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0].1.sha.as_deref(), Some("c"));
            assert!(json_files(&inbox_dir(&dir)).is_empty());
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn malformed_inbox_file_is_set_aside() {
            let dir = temp("bad");
            let inbox = inbox_dir(&dir);
            std::fs::create_dir_all(&inbox).unwrap();
            std::fs::write(inbox.join("1-1.json"), "{nope").unwrap();
            drain_inbox(&dir);
            assert!(inbox.join("1-1.json.bad").exists());
            assert!(queued(&dir).is_empty());
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn cancel_requests_are_drained_once() {
            let dir = temp("cancel");
            write_cancel(&dir, "smoke", 7).unwrap();
            write_cancel(&dir, "other-repo", 12).unwrap();
            std::fs::write(cancel_dir(&dir).join("junk"), "").unwrap();
            assert_eq!(
                drain_cancels(&dir),
                vec![("other-repo".to_owned(), 12), ("smoke".to_owned(), 7)]
            );
            assert!(drain_cancels(&dir).is_empty());
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
