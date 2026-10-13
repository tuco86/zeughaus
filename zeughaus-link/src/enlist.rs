//! The enlist protocol: a runner dials the CI, introduces itself and then takes
//! assignments on one long exchange while it streams each run's output back.
//!
//! Frames on [`crate::ENLIST_PATH`] and [`crate::ARTIFACTS_PATH`] are a 4-byte
//! little-endian length followed by a postcard body of at most
//! [`MAX_ENLIST_FRAME_BYTES`].

use serde::{Deserialize, Serialize};

use crate::machine::MachineState;

/// The protocol major a runner and the CI must share.
pub const ENLIST_MAJOR: u32 = 1;

/// The most one frame body may carry.
pub const MAX_ENLIST_FRAME_BYTES: usize = 1 << 20;

/// The most bytes of run output one [`FromRunner::Output`] carries. Below
/// [`MAX_ENLIST_FRAME_BYTES`] so the frame around it still fits.
pub const MAX_OUTPUT_CHUNK: usize = 256 << 10;

/// The largest artifact tar either side accepts.
pub const MAX_ARTIFACT_BYTES: u64 = 4 << 30;

/// What a runner says to the CI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromRunner {
    /// First frame: who the runner is and which runs it still knows.
    Hello {
        major: u32,
        name: String,
        offer: Offer,
        runs: Vec<RunReport>,
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
    /// The run left artifacts the runner still holds.
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
    /// Send the run's artifacts on [`crate::ARTIFACTS_PATH`].
    Upload {
        run: String,
    },
    /// The CI is done with the run; drop what is kept of it.
    Forget {
        run: String,
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
    /// On the CI, downloaded on [`crate::ARTIFACTS_PATH`].
    Ci { run: String },
}

/// First frame of an [`crate::ARTIFACTS_PATH`] exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactRequest {
    /// `size` raw bytes follow.
    Upload {
        run: String,
        size: u64,
    },
    Download {
        run: String,
    },
}

/// The CI's answer; after `Sending`, `size` raw bytes follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactReply {
    Stored,
    Sending { size: u64 },
    Error { message: String },
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
}
