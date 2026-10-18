//! The enlist protocol: a runner dials the CI, introduces itself and then takes
//! assignments on one long exchange while it streams each run's output back.
//!
//! Frames on [`crate::ENLIST_PATH`] and [`crate::ARTIFACTS_PATH`] are a 4-byte
//! little-endian length followed by a postcard body of at most
//! [`MAX_ENLIST_FRAME_BYTES`].
//!
//! An artifact set (a tar) or a try bundle moves on an
//! [`crate::ARTIFACTS_PATH`] exchange as chunks that follow the frame that
//! announces it: an [`ArtifactRequest::Upload`] (producer to CI) or an
//! [`ArtifactReply::Sending`] (CI to consumer). A chunk is a `u32`
//! little-endian length in `1..=`[`ARTIFACT_CHUNK`] and that many bytes; a
//! length of 0 ends the set. A body that ends without the 0 is a broken
//! transfer, and every hop refuses a running total above
//! [`MAX_ARTIFACT_BYTES`]. The size is not announced, so the sender packs
//! while it sends and the receiver unpacks while it receives.

use serde::{Deserialize, Serialize};

use crate::machine::MachineState;

/// The protocol major a runner and the CI must share.
pub const ENLIST_MAJOR: u32 = 4;

/// The most one frame body may carry.
pub const MAX_ENLIST_FRAME_BYTES: usize = 1 << 20;

/// The most bytes of run output one [`FromRunner::Output`] carries. Below
/// [`MAX_ENLIST_FRAME_BYTES`] so the frame around it still fits.
pub const MAX_OUTPUT_CHUNK: usize = 256 << 10;

/// The largest artifact set, counted in bytes of tar, either side accepts.
pub const MAX_ARTIFACT_BYTES: u64 = 4 << 30;

/// The most bytes one chunk of an artifact transfer carries.
pub const ARTIFACT_CHUNK: usize = 1 << 20;

/// What a runner says to the CI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromRunner {
    /// First frame: who the runner is, which runs it still knows and which
    /// artifact sets it holds.
    Hello {
        major: u32,
        name: String,
        offer: Offer,
        runs: Vec<RunReport>,
        /// Run ids of the artifact sets under the runner's `ci/artifacts/`.
        artifacts: Vec<String>,
    },
    /// The busy state changed.
    Busy { state: MachineState },
    /// A machine behind the runner (a VM) changed status.
    Machine { name: String, status: String },
    /// The assignment's process is running.
    Started { run: String },
    /// The assignment could not be started.
    StartFailed { run: String, message: String },
    /// A piece of the run's log starting at byte `offset`;
    /// `bytes.len() <= MAX_OUTPUT_CHUNK`.
    Output {
        run: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// The run was paused or resumed because the machine got busy or free.
    Frozen { run: String, frozen: bool },
    /// The run ended; its log has been sent up to its end.
    Finished { run: String, end: RunEnd },
    /// An input of the run `run`, the consuming job's, moves to this runner:
    /// the artifact set of its need `need`.
    Transfer {
        run: String,
        need: String,
        event: TransferEvent,
    },
}

/// How far the transfer of one input has come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferEvent {
    /// The runner began to fetch the set.
    Started,
    /// The set arrived and is unpacked; `bytes` is the size of its tar.
    Done {
        bytes: u64,
    },
    Failed {
        message: String,
    },
}

/// What a runner can run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub containers: bool,
    pub machines: Vec<String>,
}

/// A run the runner remembers across a reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    pub run: String,
    /// `None` while it still runs.
    pub end: Option<RunEnd>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnd {
    pub code: Option<i32>,
    pub killed: bool,
    pub timed_out: bool,
    /// The run left an artifact set, which the runner holds until the CI
    /// drops it.
    pub artifacts: bool,
}

/// What the CI says to a runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToRunner {
    /// Answer to `Hello`: for each reported run, how many bytes of its log
    /// the CI already holds.
    Welcome {
        major: u32,
        offsets: Vec<(String, u64)>,
    },
    /// The runner may not enlist.
    Refused {
        message: String,
    },
    Assign {
        assignment: Box<Assignment>,
    },
    Kill {
        run: String,
    },
    /// A job on another runner is downloading the run's artifacts now: send
    /// them on [`crate::ARTIFACTS_PATH`] as `transfer`.
    Upload {
        run: String,
        transfer: u64,
    },
    /// The CI is done with the run; drop what is kept of it.
    Forget {
        run: String,
    },
    /// No job needs the run's artifact set any more: delete it.
    DropArtifacts {
        run: String,
    },
    /// The job `consumer` will run on this runner and has not been assigned
    /// yet, and the run `run` left the set its need `need` is: fetch it now,
    /// as `consumer` will ask for it when it starts.
    Prefetch {
        run: String,
        consumer: String,
        need: String,
    },
    /// The job `consumer` will not run here: delete what was fetched for it.
    DropPrefetch {
        consumer: String,
    },
}

/// One job for one runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// [`run_id`] of (repo, number, job).
    pub run: String,
    pub repo: String,
    pub number: u64,
    pub job: String,
    pub url: String,
    pub fetch_user: String,
    pub fetch_token: Option<String>,
    pub sha: String,
    /// The job file under `.zeughaus-ci/`.
    pub file: String,
    pub interpreter: String,
    pub place: Place,
    pub cache: Vec<String>,
    pub env: Vec<(String, String)>,
    pub secrets: Vec<(String, String)>,
    pub timeout_minutes: u32,
    pub when_busy: WhenBusy,
    pub inputs: Vec<Input>,
    /// The directory name of the job's workspace under `ci/work/<repo>/`: the
    /// job's name, or the name of the context it shares with other jobs.
    /// Empty in an assignment a runner persisted before the field existed.
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub checkout: Checkout,
    /// A snapshot of working trees in place of a commit of the repository.
    #[serde(default)]
    pub upload: Option<Upload>,
}

/// What the launcher does to the workspace before the job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Checkout {
    /// Fetch, checkout, `git clean -ffdxq` keeping the `cache` paths.
    #[default]
    Clean,
    /// The same without keeping any path: a clean channel's first use.
    Wipe,
    /// No fetch, no checkout, no clean: an earlier job of this pipeline's
    /// context left the workspace; `.git/zci-context` must name this pipeline.
    Continue,
}

/// The working trees a try run executes in, in place of a checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upload {
    /// `dirs[0]` is the main checkout; a dir's index is its bundle's index.
    pub dirs: Vec<UploadDir>,
}

/// One checkout of an [`Upload`]: its directory name and the commit its
/// bundle holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadDir {
    pub dir: String,
    pub sha: String,
}

/// Where a job's process runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Place {
    /// On the runner's own host.
    Host,
    /// In a container built from the repository's Containerfile for `image`,
    /// tagged `tag`.
    Container { image: String, tag: String },
    /// On a machine behind the runner (a VM).
    Machine { name: String },
}

/// What a job does while its runner is busy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenBusy {
    /// Start only once the runner is free.
    #[default]
    Wait,
    /// Start, and pause while the runner is busy.
    Freeze,
    /// Start regardless.
    Run,
}

/// The artifacts of a job this one needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub need: String,
    pub source: InputSource,
}

/// Where a runner finds a needed job's artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputSource {
    /// On this runner, from a run it executed.
    Local { run: String },
    /// On the runner that executed `run`, relayed by the CI on
    /// [`crate::ARTIFACTS_PATH`].
    Ci { run: String },
}

/// First frame of an [`crate::ARTIFACTS_PATH`] exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactRequest {
    /// The producing runner answers [`ToRunner::Upload`]; the chunks of the
    /// set follow, which the CI relays to the waiting download as they
    /// arrive.
    Upload { run: String, transfer: u64 },
    /// The producing runner cannot send what `transfer` asked for.
    Unavailable {
        run: String,
        transfer: u64,
        message: String,
    },
    /// A consuming runner fetches `run`'s artifacts for a job it was assigned.
    Download { run: String },
    /// A runner fetches bundle `index` of the try run `run` it was assigned.
    TryBundle { run: String, index: u32 },
}

/// The CI's answer; after `Sending`, the chunks of the set follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactReply {
    /// An upload reached its download, or an `Unavailable` was passed on.
    Done,
    Sending,
    Error {
        message: String,
    },
}

/// The id of a job's run. Repository and job names are `[a-z0-9][a-z0-9-]*`,
/// so the dots are unambiguous.
pub fn run_id(repo: &str, number: u64, job: &str) -> String {
    format!("{repo}.{number}.{job}")
}

/// Inverse of [`run_id`].
pub fn parse_run_id(s: &str) -> Option<(String, u64, String)> {
    let mut parts = s.splitn(3, '.');
    let repo = parts.next()?;
    let number = parts.next()?.parse().ok()?;
    let job = parts.next()?;
    Some((repo.to_owned(), number, job.to_owned()))
}

/// Writes one frame. A body above [`MAX_ENLIST_FRAME_BYTES`] is refused.
#[cfg(feature = "tokio")]
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), String>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    use tokio::io::AsyncWriteExt;

    let body = postcard::to_stdvec(msg).map_err(|e| format!("encode frame: {e}"))?;
    if body.len() > MAX_ENLIST_FRAME_BYTES {
        return Err(format!(
            "frame of {} bytes exceeds {MAX_ENLIST_FRAME_BYTES}",
            body.len()
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)
        .await
        .map_err(|e| format!("write frame: {e}"))
}

/// Reads one frame. `None` is a clean end at a frame boundary; a length above
/// [`MAX_ENLIST_FRAME_BYTES`] is an error raised before the body is read.
#[cfg(feature = "tokio")]
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>, String>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    use tokio::io::{AsyncReadExt, ErrorKind};

    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]).await {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err("stream ended inside a frame length".to_owned()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read frame: {e}")),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_ENLIST_FRAME_BYTES {
        return Err(format!(
            "frame of {len} bytes exceeds {MAX_ENLIST_FRAME_BYTES}"
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|e| format!("read frame: {e}"))?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(|e| format!("decode frame: {e}"))
}

/// Writes one chunk of an artifact transfer: its length, then `bytes`, which
/// must be `1..=`[`ARTIFACT_CHUNK`] long. A length of 0 ends the set, which
/// [`write_end`] writes.
#[cfg(feature = "tokio")]
pub async fn write_chunk<W>(w: &mut W, bytes: &[u8]) -> Result<(), String>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    if bytes.is_empty() || bytes.len() > ARTIFACT_CHUNK {
        return Err(format!(
            "a chunk of {} bytes is not in 1..={ARTIFACT_CHUNK}",
            bytes.len()
        ));
    }
    // At most ARTIFACT_CHUNK, which a u32 holds.
    w.write_all(&(bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| format!("write chunk: {e}"))?;
    w.write_all(bytes)
        .await
        .map_err(|e| format!("write chunk: {e}"))
}

/// Ends an artifact transfer: the set is complete.
#[cfg(feature = "tokio")]
pub async fn write_end<W>(w: &mut W) -> Result<(), String>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    w.write_all(&0u32.to_le_bytes())
        .await
        .map_err(|e| format!("write end: {e}"))
}

/// Reads the next chunk of an artifact transfer; `buf` holds exactly that
/// chunk afterwards. `false` is the end of the set, and `buf` is empty then.
/// A stream that ends before the end marker is an error, and so is a length
/// above [`ARTIFACT_CHUNK`], raised before its body is read.
#[cfg(feature = "tokio")]
pub async fn read_chunk<R>(r: &mut R, buf: &mut Vec<u8>) -> Result<bool, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncReadExt, ErrorKind};

    let cut_short = |e: std::io::Error| {
        if e.kind() == ErrorKind::UnexpectedEof {
            "the stream ended before the end of the set".to_owned()
        } else {
            format!("read chunk: {e}")
        }
    };
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.map_err(cut_short)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 {
        buf.clear();
        return Ok(false);
    }
    if len > ARTIFACT_CHUNK {
        return Err(format!("chunk of {len} bytes exceeds {ARTIFACT_CHUNK}"));
    }
    // The read overwrites everything it keeps, so only what the buffer
    // lacks of a longer chunk is zero-filled.
    buf.truncate(len);
    buf.resize(len, 0);
    r.read_exact(buf).await.map_err(cut_short)?;
    Ok(true)
}

#[cfg(test)]
fn sample_assignment() -> Assignment {
    Assignment {
        run: "r.1.j".to_owned(),
        repo: "r".to_owned(),
        number: 1,
        job: "j".to_owned(),
        url: String::new(),
        fetch_user: String::new(),
        fetch_token: None,
        sha: "ab".to_owned(),
        file: "j.sh".to_owned(),
        interpreter: "sh".to_owned(),
        place: Place::Host,
        cache: vec![".cache".to_owned()],
        env: Vec::new(),
        secrets: Vec::new(),
        timeout_minutes: 1,
        when_busy: WhenBusy::Wait,
        inputs: Vec::new(),
        workspace: "box".to_owned(),
        checkout: Checkout::Continue,
        upload: Some(Upload {
            dirs: vec![UploadDir {
                dir: "zeughaus".to_owned(),
                sha: "ab".repeat(20),
            }],
        }),
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversize_length_is_refused_before_the_body() {
        // Only the length is present: reading a body would hit EOF and report
        // a different error.
        let len = (MAX_ENLIST_FRAME_BYTES as u32 + 1).to_le_bytes();
        let mut r: &[u8] = &len;
        let err = read_frame::<_, ToRunner>(&mut r).await.unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn frames_round_trip_and_end_cleanly() {
        let msg = FromRunner::Output {
            run: "r.1.j".into(),
            offset: 7,
            bytes: vec![1, 2, 3],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        let mut r: &[u8] = &buf;
        assert_eq!(
            read_frame::<_, FromRunner>(&mut r).await.unwrap(),
            Some(msg)
        );
        assert_eq!(read_frame::<_, FromRunner>(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn truncated_length_is_an_error() {
        let mut r: &[u8] = &[1, 0];
        assert!(read_frame::<_, ToRunner>(&mut r).await.is_err());
    }

    #[tokio::test]
    async fn an_assignment_with_a_context_and_an_upload_round_trips() {
        let msg = ToRunner::Assign {
            assignment: Box::new(sample_assignment()),
        };
        let request = ArtifactRequest::TryBundle {
            run: "r.1.j".to_owned(),
            index: 1,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        write_frame(&mut buf, &request).await.unwrap();
        let mut r: &[u8] = &buf;
        assert_eq!(read_frame::<_, ToRunner>(&mut r).await.unwrap(), Some(msg));
        assert_eq!(
            read_frame::<_, ArtifactRequest>(&mut r).await.unwrap(),
            Some(request)
        );
    }

    #[tokio::test]
    async fn chunks_round_trip_and_the_set_ends_at_the_marker() {
        let big = vec![7u8; ARTIFACT_CHUNK];
        let mut wire = Vec::new();
        write_chunk(&mut wire, b"abc").await.unwrap();
        write_chunk(&mut wire, &big).await.unwrap();
        write_chunk(&mut wire, b"z").await.unwrap();
        write_end(&mut wire).await.unwrap();
        // What follows the set belongs to whoever reads on.
        wire.extend_from_slice(b"next");

        let mut r: &[u8] = &wire;
        let mut buf = Vec::new();
        assert!(read_chunk(&mut r, &mut buf).await.unwrap());
        assert_eq!(buf, b"abc");
        assert!(read_chunk(&mut r, &mut buf).await.unwrap());
        assert_eq!(buf, big);
        assert!(read_chunk(&mut r, &mut buf).await.unwrap());
        assert_eq!(buf, b"z", "a short chunk replaces a long one");
        assert!(!read_chunk(&mut r, &mut buf).await.unwrap());
        assert!(buf.is_empty());
        assert_eq!(r, b"next");
    }

    #[tokio::test]
    async fn a_set_cut_before_its_end_marker_is_an_error() {
        let mut whole = Vec::new();
        write_chunk(&mut whole, b"abcdef").await.unwrap();
        write_end(&mut whole).await.unwrap();
        // Cut inside the length, inside the body, between body and marker
        // and inside the marker.
        for cut in 0..whole.len() {
            let mut r: &[u8] = &whole[..cut];
            let mut buf = Vec::new();
            let mut read = read_chunk(&mut r, &mut buf).await;
            if read == Ok(true) {
                read = read_chunk(&mut r, &mut buf).await;
            }
            let err = read.unwrap_err();
            assert!(err.contains("before the end"), "cut at {cut}: {err}");
        }
    }

    #[tokio::test]
    async fn an_oversize_chunk_is_refused_before_its_body() {
        // Only the length follows: a read of the body would meet the end of
        // the stream and report that instead.
        let len = (ARTIFACT_CHUNK as u32 + 1).to_le_bytes();
        let mut r: &[u8] = &len;
        let err = read_chunk(&mut r, &mut Vec::new()).await.unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn a_chunk_holds_one_byte_up_to_the_limit() {
        let mut out = Vec::new();
        // A zero length is the end of the set, so an empty chunk must not be
        // written.
        assert!(write_chunk(&mut out, &[]).await.is_err());
        assert!(
            write_chunk(&mut out, &vec![0u8; ARTIFACT_CHUNK + 1])
                .await
                .is_err()
        );
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn a_transfer_report_round_trips() {
        let events = [
            TransferEvent::Started,
            TransferEvent::Done { bytes: 3 << 30 },
            TransferEvent::Failed {
                message: "the CI is gone".to_owned(),
            },
        ];
        let mut buf = Vec::new();
        for event in &events {
            let msg = FromRunner::Transfer {
                run: "r.2.c".to_owned(),
                need: "a".to_owned(),
                event: event.clone(),
            };
            write_frame(&mut buf, &msg).await.unwrap();
        }
        let mut r: &[u8] = &buf;
        for event in events {
            let Some(FromRunner::Transfer {
                run,
                need,
                event: got,
            }) = read_frame::<_, FromRunner>(&mut r).await.unwrap()
            else {
                panic!("not a transfer report");
            };
            assert_eq!((run.as_str(), need.as_str(), got), ("r.2.c", "a", event));
        }
    }

    #[tokio::test]
    async fn the_prefetch_commands_round_trip() {
        let commands = [
            ToRunner::Prefetch {
                run: "r.2.a".to_owned(),
                consumer: "r.2.c".to_owned(),
                need: "a".to_owned(),
            },
            ToRunner::DropPrefetch {
                consumer: "r.2.c".to_owned(),
            },
        ];
        let mut buf = Vec::new();
        for command in &commands {
            write_frame(&mut buf, command).await.unwrap();
        }
        let mut r: &[u8] = &buf;
        for command in commands {
            assert_eq!(
                read_frame::<_, ToRunner>(&mut r).await.unwrap(),
                Some(command)
            );
        }
    }
}

#[cfg(test)]
mod id_tests {
    use super::*;

    #[test]
    fn run_id_round_trips() {
        let id = run_id("zeughaus", 42, "gate-linux");
        assert_eq!(
            parse_run_id(&id),
            Some(("zeughaus".to_owned(), 42, "gate-linux".to_owned()))
        );
    }

    #[test]
    fn run_id_refuses_a_non_numeric_number() {
        assert_eq!(parse_run_id("a.b.c"), None);
        assert_eq!(parse_run_id("a.1"), None);
    }

    #[test]
    fn when_busy_parses_lowercase() {
        #[derive(Deserialize)]
        struct H {
            when_busy: WhenBusy,
        }
        let h: H = serde_json::from_str(r#"{"when_busy":"freeze"}"#).unwrap();
        assert_eq!(h.when_busy, WhenBusy::Freeze);
        assert_eq!(WhenBusy::default(), WhenBusy::Wait);
    }

    /// A runner persists its assignments as JSON (`assigned/<run>.json`), so
    /// one written before contexts must still load.
    #[test]
    fn an_assignment_from_before_contexts_still_loads() {
        let mut old = serde_json::to_value(sample_assignment()).unwrap();
        let fields = old.as_object_mut().unwrap();
        for key in ["workspace", "checkout", "upload"] {
            assert!(fields.remove(key).is_some(), "{key}");
        }
        let loaded: Assignment = serde_json::from_value(old).unwrap();
        assert_eq!(loaded.workspace, "");
        assert_eq!(loaded.checkout, Checkout::Clean);
        assert_eq!(loaded.upload, None);
    }
}
