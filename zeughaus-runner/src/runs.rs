//! Serving the files of the runs this process executed.
//!
//! A run's output WAL, its exit record and its copied artifacts live under
//! `<state-dir>/runs/<run-id>/`, on the runner that produced them and nowhere
//! else, so an editor that wants to read one asks this endpoint for a range of
//! it. Range reads rather than a download: a log is written while it is read,
//! and an editor that tails it asks for what is new and is told how large the
//! file has become.
//!
//! Nothing here is logged per request. An editor paging through a long log
//! sends a request per screen, and a peer asking for a file that is not there
//! is answered, not narrated -- a line per request would bury everything else
//! the runner says.

use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use weida::{Replier, TransferMeta};
use zeughaus_link::{ActivityKind, MAX_RUN_REQUEST_BYTES, RunFileReply, RunFileRequest};

/// What `activity.json` of a run directory says about the terminal whose
/// output it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunActivity {
    pub kind: ActivityKind,
    pub label: String,
    pub terminal: u64,
    /// The grid width the output was written at, which a replay uses.
    pub cols: u16,
}

pub const ACTIVITY_FILE: &str = "activity.json";

pub fn write_activity(run_dir: &Path, activity: &RunActivity) -> Result<(), String> {
    crate::files::write_json(&run_dir.join(ACTIVITY_FILE), activity)
}

pub fn read_activity(run_dir: &Path) -> Option<RunActivity> {
    let text = std::fs::read(run_dir.join(ACTIVITY_FILE)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// The run directories under `<state-dir>/runs/`: one per job run, CI job,
/// maintenance task and closed shell or session, numbered past every
/// directory that already exists, so a restarted runner cannot hand out an
/// id whose directory holds someone else's output.
pub struct RunDirs {
    root: PathBuf,
    next: AtomicU64,
    /// Prunable runs kept beyond which older ones go when a run is added.
    keep: usize,
}

impl RunDirs {
    pub fn new(state_dir: &Path, keep: usize) -> RunDirs {
        let root = state_dir.join("runs");
        let next = run_ids(&root).first().copied().unwrap_or(0) + 1;
        RunDirs {
            root,
            next: AtomicU64::new(next),
            keep,
        }
    }

    /// A fresh run directory and its id. Housekeeping rides on it rather
    /// than on a timer: the directory only grows when a run is added.
    pub fn create(&self) -> Result<(u64, PathBuf), String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let dir = self.root.join(id.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        prune_runs(&self.root, self.keep);
        Ok((id, dir))
    }

    /// The id of `dir` when it is one of these run directories.
    pub fn id_of(&self, dir: &Path) -> Option<u64> {
        if dir.parent() != Some(self.root.as_path()) {
            return None;
        }
        dir.file_name()?.to_str()?.parse().ok()
    }

    /// Every run id, newest first.
    pub fn ids(&self) -> Vec<u64> {
        run_ids(&self.root)
    }

    pub fn dir(&self, id: u64) -> PathBuf {
        self.root.join(id.to_string())
    }
}

/// The numeric run directories under `runs`, newest id first. A name that is
/// not a number is not a run of this process and is ignored; an unreadable
/// directory means no runs are known, which is the same situation as a first
/// start.
fn run_ids(runs: &Path) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Whether the run in `dir` may go once newer ones replace it: a run that
/// exited cleanly, and the output of a closed shell or session whatever its
/// end. A failed run, one without a record yet and an unreadable one stay.
fn prunable(dir: &Path) -> bool {
    let clean = std::fs::read_to_string(dir.join("exit"))
        .is_ok_and(|record| record.starts_with("code=0\n"));
    clean
        || read_activity(dir)
            .is_some_and(|a| matches!(a.kind, ActivityKind::Shell | ActivityKind::Session))
}

/// Deletes prunable runs beyond the `keep` newest. Failures to delete are
/// logged and otherwise ignored: retention is housekeeping, and a run that
/// cannot be removed today is tried again on the next run.
fn prune_runs(runs: &Path, keep: usize) {
    let mut kept = 0usize;
    for id in run_ids(runs) {
        let dir = runs.join(id.to_string());
        if !prunable(&dir) {
            continue;
        }
        if kept < keep {
            kept += 1;
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("[runner] cannot prune run {}: {e}", dir.display());
        }
    }
}

/// Answers run-file requests until the replier goes away, which for this
/// process means never: it owns the replier for the life of the program.
pub async fn serve_runs(replier: Replier, state_dir: PathBuf) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[runner] stopped serving run files: {e}");
                return;
            }
        };
        let asked = match request.take_body().collect(MAX_RUN_REQUEST_BYTES).await {
            Ok(asked) => asked,
            Err(e) => {
                eprintln!("[runner] unreadable run request: {e}");
                continue;
            }
        };
        let answer = chunk(&state_dir, &asked).await.encode();
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[runner] cannot reply to a run request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&answer).await {
            eprintln!("[runner] cannot write a run chunk: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[runner] cannot finish a run chunk: {e}");
        }
    }
}

/// What one request reads, or an empty reply.
///
/// A malformed request, a name that would leave the run directory and a file
/// that does not exist are the same answer: `total = 0` and no bytes. The
/// requester cannot tell them apart, which is the point -- probing this
/// endpoint tells a peer nothing about the filesystem it is not allowed to
/// read anyway.
async fn chunk(state_dir: &Path, asked: &[u8]) -> RunFileReply {
    let Some(request) = RunFileRequest::decode(asked) else {
        return empty(0);
    };
    if request.validate().is_err() {
        return empty(request.offset);
    }
    let path = state_dir
        .join("runs")
        .join(request.run_id.to_string())
        .join(&request.name);
    let offset = request.offset;
    let limit = request.limit as usize;
    // On the blocking pool: the file is on disk, and the task this runs on is
    // also weida's, which must keep draining the connection.
    match tokio::task::spawn_blocking(move || read_range(&path, offset, limit)).await {
        Ok(Some((total, bytes))) => RunFileReply {
            total,
            offset,
            bytes,
        },
        Ok(None) | Err(_) => empty(offset),
    }
}

fn empty(offset: u64) -> RunFileReply {
    RunFileReply {
        total: 0,
        offset,
        bytes: Vec::new(),
    }
}

/// The file's current length and at most `limit` bytes from `offset`.
///
/// The length is taken from the open file, so a growing log answers what it
/// held at this read and the next request continues where this one stopped.
/// An offset at or past the end is not an error: that is a tail that has
/// caught up.
fn read_range(path: &Path, offset: u64, limit: usize) -> Option<(u64, Vec<u8>)> {
    let mut file = std::fs::File::open(path).ok()?;
    let total = file.metadata().ok()?.len();
    if offset >= total || limit == 0 {
        return Some((total, Vec::new()));
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let want = limit.min((total - offset) as usize);
    let mut bytes = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    bytes.truncate(filled);
    Some((total, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        fn temp_dir(tag: &str) -> PathBuf {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "zeughaus-runs-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            path
        }

        /// Paging a log is the whole contract: every request is answered with the
        /// length the file has now, so a tail knows both what it got and what is
        /// still ahead of it.
        #[test]
        fn a_range_read_answers_the_current_length() {
            let dir = temp_dir("range");
            let file = dir.join("log");
            std::fs::write(&file, b"0123456789").expect("write");

            assert_eq!(
                read_range(&file, 0, 4),
                Some((10, b"0123".to_vec())),
                "the first page"
            );
            assert_eq!(
                read_range(&file, 8, 1024),
                Some((10, b"89".to_vec())),
                "a limit past the end reads what is there"
            );
            assert_eq!(
                read_range(&file, 10, 1024),
                Some((10, Vec::new())),
                "a tail that caught up"
            );
            assert_eq!(read_range(&dir.join("exit"), 0, 16), None, "no such file");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Oldest to newest: green, red, green, no record (cut short), a
        /// failed shell's output, green, green. The two newest prunable runs
        /// stay; the older green ones and the shell beyond them go, and the
        /// failed run, the recordless one and the new one are untouched.
        #[test]
        fn only_prunable_runs_beyond_the_newest_are_pruned() {
            let state_dir = temp_dir("prune");
            let runs = state_dir.join("runs");
            let record = |id: u64, exit: Option<&str>, kind: Option<ActivityKind>| {
                let dir = runs.join(id.to_string());
                std::fs::create_dir_all(&dir).unwrap();
                if let Some(exit) = exit {
                    std::fs::write(dir.join("exit"), exit).unwrap();
                }
                if let Some(kind) = kind {
                    let activity = RunActivity {
                        kind,
                        label: "bash".to_owned(),
                        terminal: id,
                        cols: 80,
                    };
                    write_activity(&dir, &activity).unwrap();
                }
            };
            record(1, Some("code=0\nkilled=false\n"), None);
            record(2, Some("code=3\nkilled=false\n"), None);
            record(3, Some("code=0\nkilled=false\n"), None);
            record(4, None, None);
            record(5, Some("code=1\nkilled=false\n"), Some(ActivityKind::Shell));
            record(6, Some("code=0\nkilled=false\n"), None);
            record(7, Some("code=0\nkilled=false\n"), Some(ActivityKind::Job));

            let dirs = RunDirs::new(&state_dir, 2);
            let (id, fresh) = dirs.create().unwrap();
            assert_eq!((id, fresh.clone()), (8, runs.join("8")));
            assert_eq!(dirs.id_of(&fresh), Some(8));
            assert_eq!(dirs.id_of(&state_dir.join("8")), None);
            assert_eq!(dirs.ids(), vec![8, 7, 6, 4, 2]);
            // A restarted runner continues the numbering past what is on disk.
            assert_eq!(RunDirs::new(&state_dir, 2).create().unwrap().0, 9);
            let _ = std::fs::remove_dir_all(&state_dir);
        }
    }
}
