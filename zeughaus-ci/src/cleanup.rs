//! Disk housekeeping after a pipeline: old pipeline records and their
//! transcripts. Workspaces, container images and artifact sets belong to
//! the runners.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::event::CiEvent;
use crate::files::read_json;

/// Pipeline records kept per repository and channel.
const KEEP_PIPELINES: usize = 200;

/// Keeps the newest [`KEEP_PIPELINES`] pipeline records of every channel of
/// `repo` (with their transcripts under `<n>.runs/`), and always those of the
/// pipelines in `running`.
pub fn prune_pipelines(state_dir: &Path, repo: &str, running: &HashSet<u64>) {
    let dir = crate::ci_dir(state_dir).join("pipelines").join(repo);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut by_channel: HashMap<String, Vec<u64>> = HashMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(n) = name
            .strip_suffix(".json")
            .and_then(|n| n.parse::<u64>().ok())
            && let Some(channel) = record_channel(&entry.path())
        {
            by_channel.entry(channel).or_default().push(n);
        }
    }
    for mut numbers in by_channel.into_values() {
        numbers.sort_by_key(|n| std::cmp::Reverse(*n));
        for n in numbers
            .into_iter()
            .skip(KEEP_PIPELINES)
            .filter(|n| !running.contains(n))
        {
            let _ = std::fs::remove_file(dir.join(format!("{n}.json")));
            let _ = std::fs::remove_dir_all(dir.join(format!("{n}.runs")));
        }
    }
}

/// The channel of a pipeline record, reading only what decides it. A record
/// that cannot be read is left alone.
fn record_channel(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        channel: String,
        event: CiEvent,
    }
    let head: Head = read_json(path).ok()?;
    Some(if head.channel.is_empty() {
        crate::channels::fallback(&head.event)
    } else {
        head.channel
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;
        use crate::event::EventKind;

        fn write_record(dir: &Path, n: u64, channel: &str) {
            let event = CiEvent {
                repo: "r".to_owned(),
                kind: EventKind::Push,
                git_ref: "main".to_owned(),
                sha: None,
                cron: None,
                actor: "test".to_owned(),
                delivery: None,
                upload: None,
                received: 0,
            };
            let record = serde_json::json!({ "channel": channel, "event": event, "number": n });
            std::fs::write(dir.join(format!("{n}.json")), record.to_string()).unwrap();
            std::fs::create_dir_all(dir.join(format!("{n}.runs/build"))).unwrap();
        }

        #[test]
        fn keeps_the_newest_per_channel() {
            let state = std::env::temp_dir().join(format!("zh-prune-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&state);
            let dir = crate::ci_dir(&state).join("pipelines").join("r");
            std::fs::create_dir_all(&dir).unwrap();
            for n in 1..=205 {
                write_record(&dir, n, "dev");
            }
            for n in 301..=303 {
                write_record(&dir, n, "release");
            }
            // A pipeline still running survives past the cap.
            let running: HashSet<u64> = [2].into_iter().collect();
            prune_pipelines(&state, "r", &running);
            let exists = |n: u64| dir.join(format!("{n}.json")).exists();
            let runs = |n: u64| dir.join(format!("{n}.runs")).exists();
            for n in [1, 3, 4, 5] {
                assert!(!exists(n) && !runs(n), "{n} should be gone");
            }
            assert!(exists(2) && runs(2));
            for n in (6..=205).chain(301..=303) {
                assert!(exists(n) && runs(n), "{n} should be kept");
            }
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}
