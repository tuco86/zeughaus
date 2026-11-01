//! The enlist protocol: a runner dials the CI, introduces itself and then takes
//! assignments on one long exchange while it streams each run's output back.
//!
//! Frames on [`crate::ENLIST_PATH`] and [`crate::ARTIFACTS_PATH`] are a 4-byte
//! little-endian length followed by a postcard body of at most
//! [`MAX_ENLIST_FRAME_BYTES`].
//!
//! An artifact set is a [`SetManifest`]: every file of the set by path, size
//! and BLAKE3 root. Its files move as blobs, each once per hop and only when
//! the receiving end lacks it: the producer offers the manifest
//! ([`ArtifactRequest::Offer`]) and sends the blobs the CI names
//! ([`ArtifactRequest::Blobs`]); a consumer reads the manifest
//! ([`ArtifactRequest::Manifest`]) and fetches the blobs its cache lacks
//! ([`ArtifactRequest::Fetch`]). Manifests, root lists, blobs and try bundles
//! travel as chunks: a chunk is a `u32` little-endian length in
//! `1..=`[`ARTIFACT_CHUNK`] and that many bytes; a length of 0 ends the body.
//! A blob is a [`BlobHead`] frame followed by its zstd bytes as chunks. A body
//! that ends without the 0 is a broken transfer, and every hop refuses a
//! running total above [`MAX_ARTIFACT_BYTES`].

use serde::{Deserialize, Serialize};

/// The protocol major a runner and the CI must share.
pub const ENLIST_MAJOR: u32 = 9;

/// The most one frame body may carry.
pub const MAX_ENLIST_FRAME_BYTES: usize = 1 << 20;

/// The most bytes of run output one [`FromRunner::Output`] carries. Below
/// [`MAX_ENLIST_FRAME_BYTES`] so the frame around it still fits.
pub const MAX_OUTPUT_CHUNK: usize = 256 << 10;

/// The largest artifact set, counted as the sum of its file sizes, and the
/// most bytes one artifact exchange carries.
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
        /// The CI build (pipeline number) this runner runs; 0 for one built
        /// elsewhere.
        build: u64,
        runs: Vec<RunReport>,
        /// Run ids of the artifact sets under the runner's `ci/artifacts/`.
        artifacts: Vec<String>,
    },
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
    /// The set arrived and is in place: `bytes` is the sum of its file
    /// sizes, `moved` the compressed blob bytes that crossed the wire for it.
    Done {
        bytes: u64,
        moved: u64,
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
    /// What became of each store of the assignment's `state`.
    #[serde(default)]
    pub state: Vec<StateResult>,
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
    /// A job on another runner needs the run's artifacts now: offer the set
    /// of `run` to the CI as `transfer` on [`crate::ARTIFACTS_PATH`].
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
    pub env: Vec<(String, String)>,
    pub secrets: Vec<(String, String)>,
    pub timeout_minutes: u32,
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub checkout: Checkout,
    /// A snapshot of working trees in place of a commit of the repository.
    #[serde(default)]
    pub upload: Option<Upload>,
    /// The job state stores the run starts from and may save.
    #[serde(default)]
    pub state: Vec<StateMount>,
}

/// What the launcher does to the workspace before the job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Checkout {
    /// Fetch, `checkout --force`, then `git clean -ffd`: ignored files such
    /// as `target/` stay.
    #[default]
    Clean,
    /// The same with `git clean -ffdx`: nothing stays. A clean channel's
    /// first use of a workspace in a pipeline.
    Wipe,
}

/// The most stores one job keeps.
pub const MAX_STATES: usize = 8;

/// When the end of a run saves a store (`state_save` of the job header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateSave {
    /// Never saved: a manual run's stores. No header names it.
    Never,
    /// Saved when the job exits with code 0.
    #[default]
    Success,
    /// Saved after any end but a kill.
    Always,
}

impl StateSave {
    pub fn as_str(self) -> &'static str {
        match self {
            StateSave::Never => "never",
            StateSave::Success => "success",
            StateSave::Always => "always",
        }
    }
}

/// A store the run starts from (`version`, `None` while the store has none)
/// and may save when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateMount {
    pub name: String,
    pub version: Option<u64>,
    pub save: StateSave,
}

/// What became of one store at the end of a run; `from` is the version it
/// started from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateResult {
    pub name: String,
    pub from: Option<u64>,
    pub outcome: StateOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateOutcome {
    /// Saved as the pipeline's version; `bytes` is the sum of its files.
    Saved { bytes: u64 },
    /// Not saved, and that is no failure: `why` says why.
    NotSaved { why: String },
    /// The save failed, which fails the job.
    Failed { message: String },
}

/// The working trees a manual run executes in, in place of a checkout.
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

impl Place {
    /// The directory name of the workspace a repository's jobs at this place
    /// share on their runner: pipeline jobs and manual runs build in it one
    /// at a time and find what the one before left.
    pub fn workspace(&self) -> String {
        match self {
            Place::Host => "host".to_owned(),
            Place::Container { image, .. } => format!("image-{image}"),
            Place::Machine { name } => format!("machine-{name}"),
        }
    }
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
    /// On the runner that executed `run`, through the CI on
    /// [`crate::ARTIFACTS_PATH`].
    Ci { run: String },
}

/// First frame of an [`crate::ARTIFACTS_PATH`] exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactRequest {
    /// Producer, answering [`ToRunner::Upload`]: the set's encoded
    /// [`SetManifest`] follows as chunks and an end marker. Reply: `Sending`,
    /// then the concatenated roots the CI lacks ([`encode_roots`]) as chunks
    /// and an end marker; or `Error`.
    Offer { run: String, transfer: u64 },
    /// Producer, after `Offer`: for each missing root in the order the CI
    /// named, a [`BlobHead`] frame, the blob's zstd bytes as chunks and an end
    /// marker; then the request body ends. Reply: `Done` or `Error`.
    Blobs { run: String, transfer: u64 },
    /// Producer: cannot send what `transfer` asked for. Reply: `Done` or
    /// `Error`.
    Unavailable {
        run: String,
        transfer: u64,
        message: String,
    },
    /// Consumer: the set's encoded [`SetManifest`]. Reply: `Sending`, chunks,
    /// end marker; or `Error`.
    Manifest { run: String },
    /// Consumer: the concatenated roots it wants follow as chunks and an end
    /// marker. Reply: `Sending`, then per root, in that order, a [`BlobHead`]
    /// frame, the zstd bytes as chunks and an end marker; or `Error`.
    Fetch { run: String },
    /// A runner fetches bundle `index` of the manual run `run` it was
    /// assigned.
    ManualBundle { run: String, index: u32 },
    /// A runner restoring store `name` of the run `run` it was assigned:
    /// version `version`'s encoded [`SetManifest`]. Reply as to `Manifest`.
    StateManifest {
        run: String,
        name: String,
        version: u64,
    },
    /// The concatenated roots of that version it wants follow as chunks and
    /// an end marker. Reply as to `Fetch`.
    StateFetch {
        run: String,
        name: String,
        version: u64,
    },
    /// A runner saving store `name` of the run `run` it was assigned: the
    /// encoded [`SetManifest`] follows as chunks and an end marker. Reply:
    /// `Sending`, then the roots the CI lacks as chunks and an end marker
    /// (none: the version is recorded); or `Error`.
    StateOffer { run: String, name: String },
    /// After a `StateOffer` that named roots: the blobs, framed as for
    /// `Blobs`. Reply: `Done` once the version is recorded, or `Error`.
    StateBlobs { run: String, name: String },
}

/// The most entries one [`SetManifest`] holds.
pub const MAX_SET_ENTRIES: usize = 200_000;

/// The most bytes an encoded [`SetManifest`] takes.
pub const MAX_MANIFEST_BYTES: usize = 32 << 20;

/// The longest path of a [`SetEntry`].
pub const MAX_SET_PATH_BYTES: usize = 4096;

/// The files of an artifact set, parents before their children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetManifest {
    pub entries: Vec<SetEntry>,
}

/// One entry of a [`SetManifest`]. `path` is relative, its components joined
/// with `/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetEntry {
    Dir {
        path: String,
    },
    /// `root` is the BLAKE3 hash of the file's bytes.
    File {
        path: String,
        size: u64,
        root: [u8; 32],
        executable: bool,
    },
}

impl SetEntry {
    pub fn path(&self) -> &str {
        match self {
            SetEntry::Dir { path } | SetEntry::File { path, .. } => path,
        }
    }
}

/// Announces one blob in a `Blobs` request or a `Fetch` reply; its zstd
/// bytes follow as chunks and an end marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobHead {
    pub root: [u8; 32],
    /// The blob's size uncompressed.
    pub size: u64,
}

impl SetManifest {
    /// Refuses a manifest no receiver may materialize: too many entries, a
    /// path that is empty, too long, absolute or climbs out, a path twice,
    /// one root with two sizes, or more than [`MAX_ARTIFACT_BYTES`] of files.
    pub fn check(&self) -> Result<(), String> {
        use std::collections::{HashMap, HashSet};

        if self.entries.len() > MAX_SET_ENTRIES {
            return Err(format!(
                "a set of {} entries exceeds {MAX_SET_ENTRIES}",
                self.entries.len()
            ));
        }
        let mut paths = HashSet::with_capacity(self.entries.len());
        let mut sizes: HashMap<[u8; 32], u64> = HashMap::new();
        let mut total = 0u64;
        for entry in &self.entries {
            let path = entry.path();
            check_set_path(path)?;
            if !paths.insert(path) {
                return Err(format!("{path}: twice in the set"));
            }
            if let SetEntry::File { size, root, .. } = entry {
                if *sizes.entry(*root).or_insert(*size) != *size {
                    return Err(format!("{path}: its content has another size elsewhere"));
                }
                total = total.saturating_add(*size);
                if total > MAX_ARTIFACT_BYTES {
                    return Err(format!(
                        "{path}: the set exceeds {MAX_ARTIFACT_BYTES} bytes"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The sum of the file sizes.
    pub fn files(&self) -> u64 {
        self.entries
            .iter()
            .map(|entry| match entry {
                SetEntry::File { size, .. } => *size,
                SetEntry::Dir { .. } => 0,
            })
            .sum()
    }

    #[cfg(feature = "tokio")]
    pub fn encode(&self) -> Vec<u8> {
        // Serializing plain strings, integers and arrays into a Vec cannot
        // fail.
        postcard::to_stdvec(self).expect("a manifest encodes")
    }

    /// Inverse of [`SetManifest::encode`]; the result passed
    /// [`SetManifest::check`].
    #[cfg(feature = "tokio")]
    pub fn decode(bytes: &[u8]) -> Result<SetManifest, String> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "a manifest of {} bytes exceeds {MAX_MANIFEST_BYTES}",
                bytes.len()
            ));
        }
        let manifest: SetManifest =
            postcard::from_bytes(bytes).map_err(|e| format!("decode manifest: {e}"))?;
        manifest.check()?;
        Ok(manifest)
    }
}

fn check_set_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("an empty path in the set".to_owned());
    }
    if path.len() > MAX_SET_PATH_BYTES {
        return Err(format!("{path}: longer than {MAX_SET_PATH_BYTES} bytes"));
    }
    if path.starts_with('/') || path.contains(['\\', '\0', ':']) {
        return Err(format!("{path}: not a relative path of the set"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("{path}: an empty, `.` or `..` component"));
    }
    Ok(())
}

/// The roots of a `Fetch` request or an `Offer` reply, concatenated.
pub fn encode_roots(roots: &[[u8; 32]]) -> Vec<u8> {
    roots.concat()
}

/// Inverse of [`encode_roots`]; refuses a length that is no multiple of 32
/// and more than [`MAX_SET_ENTRIES`] roots.
pub fn decode_roots(bytes: &[u8]) -> Result<Vec<[u8; 32]>, String> {
    if !bytes.len().is_multiple_of(32) {
        return Err(format!("{} bytes are no list of roots", bytes.len()));
    }
    if bytes.len() / 32 > MAX_SET_ENTRIES {
        return Err(format!(
            "{} roots exceed {MAX_SET_ENTRIES}",
            bytes.len() / 32
        ));
    }
    Ok(bytes.as_chunks::<32>().0.to_vec())
}

/// The CI's answer; after `Sending`, the body the request names follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactReply {
    /// The offered blobs are stored, or an `Unavailable` was passed on.
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
        env: Vec::new(),
        secrets: Vec::new(),
        timeout_minutes: 1,
        inputs: Vec::new(),
        checkout: Checkout::Wipe,
        upload: Some(Upload {
            dirs: vec![UploadDir {
                dir: "zeughaus".to_owned(),
                sha: "ab".repeat(20),
            }],
        }),
        state: vec![StateMount {
            name: "corpus".to_owned(),
            version: Some(4),
            save: StateSave::Always,
        }],
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
        let request = ArtifactRequest::ManualBundle {
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
            TransferEvent::Done {
                bytes: 3 << 30,
                moved: 1 << 20,
            },
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

    fn file(path: &str, size: u64, byte: u8) -> SetEntry {
        SetEntry::File {
            path: path.to_owned(),
            size,
            root: [byte; 32],
            executable: false,
        }
    }

    fn dir(path: &str) -> SetEntry {
        SetEntry::Dir {
            path: path.to_owned(),
        }
    }

    fn nested() -> SetManifest {
        SetManifest {
            entries: vec![
                dir("a"),
                dir("a/b"),
                file("a/b/c.txt", 3, 1),
                file("a/same", 3, 1),
                file("top", 0, 2),
            ],
        }
    }

    #[test]
    fn a_nested_tree_passes_the_check() {
        nested().check().unwrap();
        assert_eq!(nested().files(), 6);
    }

    #[test]
    fn a_path_that_leaves_the_set_or_repeats_is_refused() {
        for bad in ["/a", "a/../b", "a//b", "a\\b", "c:x", "./a", "a/", ""] {
            let manifest = SetManifest {
                entries: vec![file(bad, 1, 1)],
            };
            assert!(manifest.check().is_err(), "{bad:?} passed");
        }
        let twice = SetManifest {
            entries: vec![dir("a"), file("a", 1, 1)],
        };
        assert!(twice.check().unwrap_err().contains("twice"));
    }

    #[test]
    fn one_root_with_two_sizes_is_refused() {
        let manifest = SetManifest {
            entries: vec![file("a", 1, 7), file("b", 2, 7)],
        };
        let err = manifest.check().unwrap_err();
        assert!(err.starts_with("b:"), "{err}");
    }

    #[test]
    fn a_set_above_the_artifact_limit_is_refused() {
        let manifest = SetManifest {
            entries: vec![file("a", MAX_ARTIFACT_BYTES, 1), file("b", 1, 2)],
        };
        assert!(manifest.check().unwrap_err().contains("exceeds"));
        let at_limit = SetManifest {
            entries: vec![file("a", MAX_ARTIFACT_BYTES - 1, 1), file("b", 1, 2)],
        };
        at_limit.check().unwrap();
    }

    #[test]
    fn a_manifest_round_trips_and_a_truncated_one_is_refused() {
        let bytes = nested().encode();
        assert_eq!(SetManifest::decode(&bytes).unwrap(), nested());
        assert!(SetManifest::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn a_decoded_manifest_is_checked() {
        let bad = SetManifest {
            entries: vec![file("../x", 1, 1)],
        };
        assert!(SetManifest::decode(&bad.encode()).is_err());
    }

    #[test]
    fn roots_round_trip_and_a_partial_root_is_refused() {
        let roots = [[1u8; 32], [2u8; 32]];
        assert_eq!(decode_roots(&encode_roots(&roots)).unwrap(), roots);
        assert_eq!(decode_roots(&[]).unwrap(), Vec::<[u8; 32]>::new());
        assert!(decode_roots(&[0u8; 33]).is_err());
    }

    #[tokio::test]
    async fn a_blob_head_and_the_artifact_requests_round_trip() {
        let head = BlobHead {
            root: [9; 32],
            size: 1 << 40,
        };
        let run = || "r.1.j".to_owned();
        let requests = [
            ArtifactRequest::Offer {
                run: run(),
                transfer: 3,
            },
            ArtifactRequest::Blobs {
                run: run(),
                transfer: 3,
            },
            ArtifactRequest::Unavailable {
                run: run(),
                transfer: 3,
                message: "gone".to_owned(),
            },
            ArtifactRequest::Manifest { run: run() },
            ArtifactRequest::Fetch { run: run() },
        ];
        let mut buf = Vec::new();
        write_frame(&mut buf, &head).await.unwrap();
        for request in &requests {
            write_frame(&mut buf, request).await.unwrap();
        }
        let mut r: &[u8] = &buf;
        assert_eq!(read_frame::<_, BlobHead>(&mut r).await.unwrap(), Some(head));
        for request in requests {
            assert_eq!(
                read_frame::<_, ArtifactRequest>(&mut r).await.unwrap(),
                Some(request)
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

    /// A runner persists its assignments as JSON (`assigned/<run>.json`), so
    /// one written before places named workspaces, before checkouts and
    /// uploads, or while jobs had a `when_busy`, must still load.
    #[test]
    fn an_assignment_from_an_older_runner_still_loads() {
        let mut old = serde_json::to_value(sample_assignment()).unwrap();
        let fields = old.as_object_mut().unwrap();
        for key in ["checkout", "upload"] {
            assert!(fields.remove(key).is_some(), "{key}");
        }
        fields.insert("workspace".to_owned(), "linux".into());
        fields.insert("cache".to_owned(), serde_json::json!([".cache"]));
        fields.insert("when_busy".to_owned(), "freeze".into());
        let loaded: Assignment = serde_json::from_value(old).unwrap();
        assert_eq!(loaded.checkout, Checkout::Clean);
        assert_eq!(loaded.upload, None);
    }
}
