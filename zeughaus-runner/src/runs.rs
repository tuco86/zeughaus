//! Serving the files of the runs this process executed.
//!
//! A run's log, its exit record and its copied artifacts live under
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

use weida::{Replier, TransferMeta};
use zeughaus_link::{MAX_RUN_REQUEST_BYTES, RunFileReply, RunFileRequest};

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
}
