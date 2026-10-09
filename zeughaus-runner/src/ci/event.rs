//! CI events and the on-disk queue.
//!
//! An event is written to `ci/inbox/` by a webhook or `ci run`, then moved to
//! `ci/queue/<key>.json` by the scheduler. The key is the repository, the
//! event kind and the ref, so a newer event for the same ref replaces the one
//! still waiting; a running pipeline is never cancelled by it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ci_dir;
use crate::files::{read_json, write_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Push,
    Tag,
    Cron,
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EventKind::Push => "push",
            EventKind::Tag => "tag",
            EventKind::Cron => "cron",
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
            other => Err(format!("unknown event kind `{other}` (push, tag or cron)")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiEvent {
    pub repo: String,
    pub kind: EventKind,
    /// Branch or tag name without the `refs/` prefix.
    pub git_ref: String,
    pub sha: Option<String>,
    /// The normalized cron expression of a cron event.
    pub cron: Option<String>,
    pub actor: String,
    pub delivery: Option<String>,
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

/// `<repo>.<kind>.<first 12 hex of sha256(git_ref \n cron)>`.
pub fn queue_key(event: &CiEvent) -> String {
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
            received,
        }
    }

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
    fn kind_round_trips() {
        for kind in [EventKind::Push, EventKind::Tag, EventKind::Cron] {
            assert_eq!(kind.to_string().parse::<EventKind>().unwrap(), kind);
        }
        assert!("pull".parse::<EventKind>().is_err());
    }
}
