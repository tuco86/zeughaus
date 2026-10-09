//! Run files: what an editor asks the runner that produced a run, and the
//! hold switch that decides whether new runs start at all.
//!
//! A run's bytes never travel through the graph link and never travel between
//! runners: the log, the exit record and the copied artifacts are files under
//! the state directory of the process that executed the run, and the only way
//! to see them is to ask that process. That is what keeps a 400 MB build log
//! out of the document and out of every editor that did not open it, and it is
//! what makes several runners work without a shared filesystem -- each serves
//! its own runs.
//!
//! So a fetch is a range read, not a download: an editor asks for `limit`
//! bytes at `offset`, is told the current size, and pages or tails from there.

use serde::{Deserialize, Serialize};

/// One range read of one file of one run.
///
/// `name` is a name inside the run directory, never a path the runner may
/// follow: [`RunFileRequest::validate`] is what stands between a peer and the
/// rest of the filesystem, so the reading side calls it before it resolves
/// anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFileRequest {
    pub run_id: u64,
    /// `"log"`, `"exit"` or `"artifacts/<relative path>"`; no `..`, no
    /// absolute path.
    pub name: String,
    pub offset: u64,
    /// At most [`MAX_RUN_CHUNK_BYTES`].
    pub limit: u32,
}

impl RunFileRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    /// Refuses everything that is not a file of this run.
    ///
    /// The run directory is the whole namespace: two fixed names and whatever
    /// was copied into `artifacts/`. A component-wise check rather than a
    /// canonicalisation, because it must refuse before any path touches the
    /// filesystem -- and because a symlink the run itself wrote is a question
    /// for the reader, not for a request that is already malformed.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.limit > MAX_RUN_CHUNK_BYTES {
            return Err("a run chunk is larger than the cap");
        }
        if self.name.contains('\0') {
            return Err("a run file name cannot contain NUL");
        }
        match self.name.as_str() {
            "log" | "exit" => Ok(()),
            name => {
                let Some(rel) = name.strip_prefix("artifacts/") else {
                    return Err("a run holds log, exit and artifacts/<path>");
                };
                if rel.is_empty() {
                    return Err("an artifact path is empty");
                }
                if rel.split('/').any(|part| part.is_empty() || part == "..") {
                    return Err("an artifact path cannot leave the run");
                }
                Ok(())
            }
        }
    }
}

/// The bytes at `offset`, and how large the file was when they were read.
///
/// Framed by hand rather than as JSON: the body is a log, and base64 in JSON
/// would cost a third of it plus an encode and a decode per chunk. `total` is
/// what lets an editor tail -- a growing file answers a fresh `total` on every
/// request, and a run that is over stops moving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFileReply {
    pub total: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl RunFileReply {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.bytes.len());
        out.extend_from_slice(&self.total.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        let total = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let offset = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        Some(Self {
            total,
            offset,
            bytes: bytes[16..].to_vec(),
        })
    }
}

/// Largest chunk one fetch moves. A megabyte is a screenful of scrollback many
/// times over, and it bounds what one request makes either end allocate.
pub const MAX_RUN_CHUNK_BYTES: u32 = 1 << 20;

/// Largest run-file request the runner reads. It carries a run id and a name.
pub const MAX_RUN_REQUEST_BYTES: usize = 4096;

/// Largest run-file reply an editor reads: one chunk plus its header.
pub const MAX_RUN_REPLY_BYTES: usize = MAX_RUN_CHUNK_BYTES as usize + 16;

/// Holding a runner, or releasing it.
///
/// A held runner starts no run and lets the live ones finish, so this is also
/// how a stop is prepared: hold, watch [`HoldReply::live_runs`] reach zero,
/// then stop without killing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldRequest {
    pub held: bool,
}

impl HoldRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// What the runner holds after the request: the state it is now in, and how
/// many runs are still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldReply {
    pub held: bool,
    pub live_runs: u32,
}

impl HoldReply {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Largest hold exchange either end reads. Two fields.
pub const MAX_HOLD_BYTES: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(name: &str) -> RunFileRequest {
        RunFileRequest {
            run_id: 7,
            name: name.to_owned(),
            offset: 0,
            limit: 4096,
        }
    }

    /// The whole point of the check: a name is a name, and every shape that
    /// would reach outside the run directory is refused before it is resolved.
    #[test]
    fn a_name_that_leaves_the_run_is_refused() {
        for name in [
            "../x",
            "artifacts/../../etc/passwd",
            "artifacts/",
            "/etc/passwd",
            "artifacts//x",
            "logs",
            "artifacts/a/../../b",
        ] {
            assert!(ask(name).validate().is_err(), "accepted {name}");
        }
    }

    #[test]
    fn the_files_a_run_has_are_accepted() {
        for name in ["log", "exit", "artifacts/a/b.txt"] {
            assert!(ask(name).validate().is_ok(), "refused {name}");
        }
    }

    #[test]
    fn an_oversized_chunk_is_refused() {
        let mut request = ask("log");
        request.limit = MAX_RUN_CHUNK_BYTES + 1;
        assert!(request.validate().is_err());
    }

    #[test]
    fn a_run_file_request_round_trips() {
        let request = RunFileRequest {
            run_id: 12,
            name: "artifacts/report.json".to_owned(),
            offset: 4096,
            limit: 65536,
        };
        assert_eq!(RunFileRequest::decode(&request.encode()), Some(request));
    }

    /// The framing is hand-written, so the header has to survive the round trip
    /// and a truncated one has to be refused rather than indexed into.
    #[test]
    fn a_run_file_reply_round_trips_and_refuses_a_short_header() {
        let reply = RunFileReply {
            total: 1 << 40,
            offset: 17,
            bytes: b"cargo build\r\n".to_vec(),
        };
        assert_eq!(RunFileReply::decode(&reply.encode()), Some(reply));
        assert!(RunFileReply::decode(&[1, 2, 3]).is_none());
        let empty = RunFileReply {
            total: 0,
            offset: 0,
            bytes: Vec::new(),
        };
        assert_eq!(RunFileReply::decode(&empty.encode()), Some(empty));
    }

    #[test]
    fn a_hold_exchange_round_trips() {
        let request = HoldRequest { held: true };
        assert_eq!(HoldRequest::decode(&request.encode()), Some(request));
        let reply = HoldReply {
            held: true,
            live_runs: 3,
        };
        assert_eq!(HoldReply::decode(&reply.encode()), Some(reply));
    }
}
