//! Artifact sets crossing to and from the CI, file by file.
//!
//! An artifact set lives on the runner that produced it, under
//! `ci/artifacts/<run>`, until the CI says `DropArtifacts`. A job that needs
//! another job's set and runs on another runner gets it through the CI's
//! `/artifacts` service, which keeps the set's files by content
//! (`zeughaus_link::enlist` has the exchanges and their framing):
//!
//! * When the CI lacks a set it says `Upload`; the producer hashes the set
//!   into a manifest and `Offer`s it, and sends the files the CI names, each
//!   compressed, as `Blobs`. A file the CI holds from an earlier set never
//!   leaves this runner again.
//! * A consumer reads the set's `Manifest`, `Fetch`es the files its blob
//!   cache (`ci/blobs`) lacks, and copies the set out of the cache into its
//!   destination. The set is recorded under `ci/blob-sets`, which keeps its
//!   files in the cache for the next pipeline.
//!
//! A blob is never a file in transit on this side's wire queue: the
//! producer's upload thread compresses into a [`ChunkWriter`] while a task
//! sends what it fills; the consumer's task receives, and its thread writes
//! each chunk into the cache as it arrives. A short queue sits between thread
//! and task at both ends, so reading and writing files overlaps with moving
//! bytes.
//!
//! The bundles of a manual run come from the CI itself, which stored them
//! (`ManualBundle`), straight into the run's own directory: they go with it.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weida::{OutgoingTransfer, ReplyStream, TransferMeta};
use zeughaus_blobs::{Root, compress_file, manifest_of, materialize, missing, record_set};
use zeughaus_link::ARTIFACTS_PATH;
use zeughaus_link::enlist::{
    ARTIFACT_CHUNK, ArtifactReply, ArtifactRequest, Assignment, BlobHead, FromRunner,
    MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_SET_ENTRIES, SetEntry, SetManifest, StateMount,
    TransferEvent, decode_roots, encode_roots, read_chunk, read_frame, run_id, write_chunk,
    write_end, write_frame,
};

use super::cleanup::remove_tree;
use super::run::safe_run;
use super::{Executor, artifacts_dir, blob_sets_dir};

/// Chunks that wait between the thread that reads or writes files and the
/// task that moves the bytes.
const QUEUE: usize = 4;

/// The zstd level of an upload. The uplink is the bottleneck, and level 9
/// still compresses at tens of MB/s.
const UPLOAD_LEVEL: i32 = 9;

/// What the thread that compresses blobs hands to the task that sends them.
enum Piece {
    /// The next blob begins.
    Head(BlobHead),
    Bytes(Vec<u8>),
    /// The blob is complete.
    BlobEnd,
    /// Every blob is sent.
    End,
}

/// What the task that receives hands to the thread that writes.
enum Received {
    /// The next blob begins.
    Head(BlobHead),
    Bytes(Vec<u8>),
    /// The blob's end marker arrived.
    BlobEnd,
    /// The end marker of the body arrived, or after the last blob, its end.
    End,
}

/// How the body of a reply after `Sending` is framed.
#[derive(Clone, Copy)]
enum Body {
    /// Chunks up to one end marker.
    Chunks,
    /// Blobs: a [`BlobHead`] frame, chunks and an end marker each, until the
    /// reply ends.
    Blobs,
}

/// What fetching one input moved.
pub(super) struct Fetched {
    /// The sum of the set's file sizes.
    pub bytes: u64,
    /// The compressed bytes that crossed the wire.
    pub moved: u64,
}

impl Executor {
    /// The run ids of the artifact sets this runner holds, for `Hello`.
    pub(super) fn artifact_sets(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(artifacts_dir(&self.state_dir)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|run| safe_run(run))
            .collect()
    }

    /// Deletes the artifact set of `run`: no job needs it any more. Runs on
    /// a thread of its own, as removing a big tree can take a while.
    pub fn drop_artifacts(self: &Arc<Self>, run: &str) {
        if !safe_run(run) {
            eprintln!("[enlist] {run:?}: not a run id; artifacts not dropped");
            return;
        }
        let dir = artifacts_dir(&self.state_dir).join(run);
        let run = run.to_owned();
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-drop".into())
            .spawn(move || match remove_tree(&dir) {
                Ok(()) => eprintln!("[enlist] {run}: artifacts dropped"),
                Err(e) => eprintln!("[enlist] {run}: cannot drop artifacts: {e}"),
            });
        if let Err(e) = spawned {
            eprintln!("[enlist] cannot start the drop thread: {e}");
        }
    }

    /// Offers the artifacts of `run` to the CI as `transfer` and sends the
    /// files it lacks, on a thread of its own.
    pub fn upload(self: &Arc<Self>, run: String, transfer: u64) {
        let exec = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-upload".into())
            .spawn(move || {
                if let Err(e) = exec.upload_blocking(&run, transfer) {
                    eprintln!("[enlist] {run}: artifacts not sent: {e}");
                }
            });
        if let Err(e) = spawned {
            eprintln!("[enlist] cannot start the upload thread: {e}");
        }
    }

    /// Hashes the set and offers it, then compresses on this thread what the
    /// CI lacks while a task sends what is compressed.
    fn upload_blocking(self: &Arc<Self>, run: &str, transfer: u64) -> Result<(), String> {
        let listed = self.set_dir(run).and_then(|dir| manifest_of(&dir));
        let (manifest, sources) = match listed {
            Ok(listed) => listed,
            Err(e) => {
                self.report_unavailable(run, transfer, &e);
                return Err(e);
            }
        };
        let offer = ArtifactRequest::Offer {
            run: run.to_owned(),
            transfer,
        };
        let wanted = self
            .handle
            .block_on(self.offer_manifest(offer, &manifest))?;
        if wanted.is_empty() {
            return Ok(());
        }
        let blobs = match blobs_for(&wanted, &manifest, &sources, run) {
            Ok(blobs) => blobs,
            Err(e) => {
                self.report_unavailable(run, transfer, &e);
                return Err(e);
            }
        };
        let request = ArtifactRequest::Blobs {
            run: run.to_owned(),
            transfer,
        };
        self.compress_and_send(request, &blobs, run)
    }

    /// Saves the directory `dir` as the store `name` of the run `run`: hashes
    /// it, offers its manifest and sends the files the CI lacks. Answers the
    /// sum of its file sizes. Called on the thread that finishes a run.
    pub(super) fn send_state(
        self: &Arc<Self>,
        run: &str,
        name: &str,
        dir: &Path,
    ) -> Result<u64, String> {
        let (manifest, sources) = manifest_of(dir)?;
        let offer = ArtifactRequest::StateOffer {
            run: run.to_owned(),
            name: name.to_owned(),
        };
        let wanted = self
            .handle
            .block_on(self.offer_manifest(offer, &manifest))?;
        if !wanted.is_empty() {
            let what = format!("state {name}");
            let blobs = blobs_for(&wanted, &manifest, &sources, &what)?;
            let request = ArtifactRequest::StateBlobs {
                run: run.to_owned(),
                name: name.to_owned(),
            };
            self.compress_and_send(request, &blobs, &what)?;
        }
        Ok(manifest.files())
    }

    /// Compresses `blobs` on this thread while a task sends them as
    /// `request`, then waits for the CI's verdict.
    fn compress_and_send(
        self: &Arc<Self>,
        request: ArtifactRequest,
        blobs: &[(BlobHead, PathBuf)],
        what: &str,
    ) -> Result<(), String> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let exec = Arc::clone(self);
        let sender = self
            .handle
            .spawn(async move { exec.send_stream(request, rx).await });
        let packed = compress_blobs(blobs, tx);
        let sent = self
            .handle
            .block_on(sender)
            .unwrap_or_else(|e| Err(format!("the sending task failed: {e}")));
        match packed {
            Ok(()) => sent,
            // The writer fails like this only when the sender stopped first,
            // and the sender's error says why.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                sent.and(Err(format!("cannot compress {what}: {e}")))
            }
            Err(e) => Err(format!("cannot compress {what}: {e}")),
        }
    }

    /// The CI holds a fill waiting for this transfer: tell it now rather than
    /// let it time out.
    fn report_unavailable(&self, run: &str, transfer: u64, message: &str) {
        if let Err(report) = self
            .handle
            .block_on(self.send_unavailable(run, transfer, message))
        {
            eprintln!("[enlist] {run}: cannot report the set unavailable: {report}");
        }
    }

    /// Offers `manifest` as `request` (`Offer` or `StateOffer`); the CI
    /// answers with the roots it lacks.
    async fn offer_manifest(
        &self,
        request: ArtifactRequest,
        manifest: &SetManifest,
    ) -> Result<Vec<Root>, String> {
        let (mut out, reply) = self.open_exchange().await?;
        write_frame(&mut out, &request).await?;
        for chunk in manifest.encode().chunks(ARTIFACT_CHUNK) {
            write_chunk(&mut out, chunk).await?;
        }
        write_end(&mut out).await?;
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Sending) => {}
            Some(ArtifactReply::Error { message }) => return Err(message),
            Some(ArtifactReply::Done) | None => {
                return Err("the CI did not answer the offer".to_owned());
            }
        }
        let mut roots = Vec::new();
        let mut chunk = Vec::new();
        while read_chunk(&mut body, &mut chunk).await? {
            if roots.len() + chunk.len() > 32 * MAX_SET_ENTRIES {
                return Err("the CI asked for more blobs than a set holds".to_owned());
            }
            roots.extend_from_slice(&chunk);
        }
        decode_roots(&roots)
    }

    /// The directory of `run`'s artifact set.
    fn set_dir(&self, run: &str) -> Result<PathBuf, String> {
        if !safe_run(run) {
            return Err("not a run id".to_owned());
        }
        let dir = artifacts_dir(&self.state_dir).join(run);
        if !dir.is_dir() {
            return Err(format!("{} does not exist", dir.display()));
        }
        Ok(dir)
    }

    /// Opens one exchange on the CI's artifact service.
    async fn open_exchange(&self) -> Result<(OutgoingTransfer, ReplyStream), String> {
        let requester = self.runtime.requester(self.tls.clone());
        let url = self.config.ci_url(ARTIFACTS_PATH)?;
        requester
            .connect(&url)
            .await
            .map_err(|e| format!("connect {url}: {e}"))?;
        requester
            .open(TransferMeta::default())
            .await
            .map_err(|e| format!("open: {e}"))
    }

    /// Tells the CI that `transfer` cannot be served.
    async fn send_unavailable(
        &self,
        run: &str,
        transfer: u64,
        message: &str,
    ) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        let request = ArtifactRequest::Unavailable {
            run: run.to_owned(),
            transfer,
            message: message.to_owned(),
        };
        write_frame(&mut out, &request).await?;
        out.finish().map_err(|e| format!("finish: {e}"))?;
        verdict(reply, "report").await
    }

    /// Opens the exchange `request` (`Blobs` or `StateBlobs`) and sends what
    /// `pieces` yields, up to the last blob.
    async fn send_stream(
        &self,
        request: ArtifactRequest,
        mut pieces: mpsc::Receiver<Piece>,
    ) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        write_frame(&mut out, &request).await?;
        let mut ended = false;
        while let Some(piece) = pieces.recv().await {
            match piece {
                Piece::Head(head) => write_frame(&mut out, &head).await?,
                Piece::Bytes(bytes) => write_chunk(&mut out, &bytes).await?,
                Piece::BlobEnd => write_end(&mut out).await?,
                Piece::End => {
                    ended = true;
                    break;
                }
            }
        }
        // Blobs that were never completed end the body early: the CI sees a
        // blob cut short, or fewer blobs than it asked for.
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let confirmed = verdict(reply, "upload").await;
        if !ended {
            return Err("compressing the set failed".to_owned());
        }
        confirmed
    }

    /// Fetches the set of `run` into `dest` as the input `need` of the job
    /// `consumer`, and tells the CI how it went: the transfer is what an
    /// editor shows of a job that waits for its inputs.
    pub(super) fn fetch_input(
        self: &Arc<Self>,
        consumer: &str,
        need: &str,
        run: &str,
        dest: &Path,
    ) -> Result<Fetched, String> {
        let report = |event| {
            self.send(FromRunner::Transfer {
                run: consumer.to_owned(),
                need: need.to_owned(),
                event,
            });
        };
        report(TransferEvent::Started);
        let fetched = self.download(run, dest);
        report(match &fetched {
            Ok(fetched) => TransferEvent::Done {
                bytes: fetched.bytes,
                moved: fetched.moved,
            },
            Err(message) => TransferEvent::Failed {
                message: message.clone(),
            },
        });
        fetched
    }

    /// Fetches the artifacts of `run` through the CI: its manifest, then the
    /// blobs the cache lacks, and copies the set out of the cache into
    /// `dest`. Called on the thread that prepares a job.
    fn download(self: &Arc<Self>, run: &str, dest: &Path) -> Result<Fetched, String> {
        let manifest = ArtifactRequest::Manifest {
            run: run.to_owned(),
        };
        let fetch = ArtifactRequest::Fetch {
            run: run.to_owned(),
        };
        self.download_set(manifest, fetch, run, dest)
    }

    /// Restores version `version` of the store `mount` of the run `a` into
    /// `dest`. The blob cache keeps the version under a run id of its own,
    /// so the next run of the job moves only what changed since.
    pub(super) fn restore_state(
        self: &Arc<Self>,
        a: &Assignment,
        mount: &StateMount,
        version: u64,
        dest: &Path,
    ) -> Result<Fetched, String> {
        let manifest = ArtifactRequest::StateManifest {
            run: a.run.clone(),
            name: mount.name.clone(),
            version,
        };
        let fetch = ArtifactRequest::StateFetch {
            run: a.run.clone(),
            name: mount.name.clone(),
            version,
        };
        let cache_id = run_id(&a.repo, version, &format!("state-{}", mount.name));
        self.download_set(manifest, fetch, &cache_id, dest)
    }

    /// Reads a set's manifest with `manifest`, fetches the blobs the cache
    /// lacks with `fetch`, copies the set out of the cache into `dest` and
    /// records it in the cache as `cache_id`.
    fn download_set(
        self: &Arc<Self>,
        manifest: ArtifactRequest,
        fetch: ArtifactRequest,
        cache_id: &str,
        dest: &Path,
    ) -> Result<Fetched, String> {
        let (rx, task) = self.receive(manifest, None, Body::Chunks);
        let mut reader = ChunkReader::new(rx);
        let read = read_to_limit(&mut reader, MAX_MANIFEST_BYTES);
        drop(reader);
        self.finish_receiving(task);
        let manifest = SetManifest::decode(&read?)?;

        let wanted = missing(&manifest, &self.blobs);
        let mut moved = 0;
        if !wanted.is_empty() {
            let roots: Vec<Root> = wanted.iter().map(|(root, _)| *root).collect();
            let (rx, task) = self.receive(fetch, Some(encode_roots(&roots)), Body::Blobs);
            let stored = store_blobs(&self.blobs, &wanted, rx);
            self.finish_receiving(task);
            moved = stored?;
        }
        let bytes = materialize(&manifest, &self.blobs, dest)?;
        if let Err(e) = record_set(&blob_sets_dir(&self.state_dir), cache_id, &manifest) {
            eprintln!("[enlist] {cache_id}: the blob cache does not keep the set: {e}");
        }
        Ok(Fetched { bytes, moved })
    }

    /// Fetches bundle `index` of the manual run `run` from the CI, which stored
    /// it when the run was uploaded, into the file `dest`. The directory of
    /// `dest` is the run's own and goes with it, so a failed transfer leaves
    /// nothing to clean up. Called on the thread that prepares a run.
    pub fn fetch_manual_bundle(
        self: &Arc<Self>,
        run: &str,
        index: u32,
        dest: &Path,
    ) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let request = ArtifactRequest::ManualBundle {
            run: run.to_owned(),
            index,
        };
        let (rx, task) = self.receive(request, None, Body::Chunks);
        let mut reader = ChunkReader::new(rx);
        let saved = save(&mut reader, dest);
        drop(reader);
        self.finish_receiving(task);
        saved
    }

    /// Starts the exchange for `request` on a task of the runtime, with
    /// `upload` as chunks after the request frame. Returns the queue of what
    /// the CI answers with, read as `mode` says, and the task that feeds it.
    fn receive(
        self: &Arc<Self>,
        request: ArtifactRequest,
        upload: Option<Vec<u8>>,
        mode: Body,
    ) -> (mpsc::Receiver<Result<Received, String>>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let exec = Arc::clone(self);
        let task = self
            .handle
            .spawn(async move { exec.receive_stream(request, upload, mode, tx).await });
        (rx, task)
    }

    /// Ends the task of [`Executor::receive`] and waits until it is gone and
    /// its streams are released. Its reader is done: it has the whole set, or
    /// it failed, and then the task may be waiting for the CI, which can take
    /// minutes, for nothing. A task that failed told the reader through its
    /// queue, so the result of the join has nothing to add.
    fn finish_receiving(&self, task: JoinHandle<()>) {
        task.abort();
        let _ = self.handle.block_on(task);
    }

    /// Asks the CI for what `request` names and passes its chunks on to the
    /// reader on `tx`. Whatever goes wrong is passed on too, as the last
    /// message.
    async fn receive_stream(
        &self,
        request: ArtifactRequest,
        upload: Option<Vec<u8>>,
        mode: Body,
        tx: mpsc::Sender<Result<Received, String>>,
    ) {
        if let Err(message) = self.pass_chunks(request, upload, mode, &tx).await {
            let _ = tx.send(Err(message)).await;
        }
    }

    async fn pass_chunks(
        &self,
        request: ArtifactRequest,
        upload: Option<Vec<u8>>,
        mode: Body,
        tx: &mpsc::Sender<Result<Received, String>>,
    ) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        write_frame(&mut out, &request).await?;
        if let Some(upload) = upload {
            for chunk in upload.chunks(ARTIFACT_CHUNK) {
                write_chunk(&mut out, chunk).await?;
            }
            write_end(&mut out).await?;
        }
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Sending) => {}
            Some(ArtifactReply::Error { message }) => return Err(message),
            Some(ArtifactReply::Done) | None => {
                return Err("the CI did not answer the request".to_owned());
            }
        }
        let mut chunk = Vec::new();
        let mut total = 0u64;
        // Each send fails only when the reader is gone: nobody wants the rest.
        loop {
            if let Body::Blobs = mode {
                match read_frame::<_, BlobHead>(&mut body).await? {
                    Some(head) => {
                        if tx.send(Ok(Received::Head(head))).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => break,
                }
            }
            while read_chunk(&mut body, &mut chunk).await? {
                total += chunk.len() as u64;
                if total > MAX_ARTIFACT_BYTES {
                    return Err(format!("more than {MAX_ARTIFACT_BYTES} bytes"));
                }
                let bytes = Received::Bytes(std::mem::take(&mut chunk));
                if tx.send(Ok(bytes)).await.is_err() {
                    return Ok(());
                }
            }
            match mode {
                Body::Chunks => break,
                Body::Blobs => {
                    if tx.send(Ok(Received::BlobEnd)).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        let _ = tx.send(Ok(Received::End)).await;
        Ok(())
    }
}

/// Reads the CI's verdict on an upload or on an `Unavailable`.
async fn verdict(reply: ReplyStream, what: &str) -> Result<(), String> {
    let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
    match read_frame::<_, ArtifactReply>(&mut body).await? {
        Some(ArtifactReply::Done) => Ok(()),
        Some(ArtifactReply::Error { message }) => Err(message),
        Some(ArtifactReply::Sending) | None => Err(format!("the CI did not confirm the {what}")),
    }
}

/// The blob of each of `wanted` with the file it is read from, each a file
/// of `manifest` (the set `what`).
fn blobs_for(
    wanted: &[Root],
    manifest: &SetManifest,
    sources: &BTreeMap<Root, PathBuf>,
    what: &str,
) -> Result<Vec<(BlobHead, PathBuf)>, String> {
    let sizes: HashMap<Root, u64> = manifest
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SetEntry::File { root, size, .. } => Some((*root, *size)),
            SetEntry::Dir { .. } => None,
        })
        .collect();
    wanted
        .iter()
        .map(|root| match (sizes.get(root), sources.get(root)) {
            (Some(&size), Some(path)) => Ok((BlobHead { root: *root, size }, path.clone())),
            _ => Err(format!(
                "the CI asked for {}, which is no file of {what}",
                hex::encode(root)
            )),
        })
        .collect()
}

/// Compresses each blob into the pieces of `tx`: its head, its chunks and its
/// end, then the end of all of them. Blocks while the sender is behind, so it
/// runs on a thread, never in a task.
fn compress_blobs(blobs: &[(BlobHead, PathBuf)], tx: mpsc::Sender<Piece>) -> io::Result<()> {
    let mut writer = ChunkWriter::new(tx);
    for (head, path) in blobs {
        writer.send(Piece::Head(*head))?;
        compress_file(path, UPLOAD_LEVEL, &mut writer)?;
        writer.end_blob()?;
    }
    writer.send(Piece::End)
}

/// Writes the blobs the receiving task passes on into `store`, each checked
/// against the one asked for next. Returns the compressed bytes received.
fn store_blobs(
    store: &zeughaus_blobs::Store,
    wanted: &[(Root, u64)],
    mut rx: mpsc::Receiver<Result<Received, String>>,
) -> Result<u64, String> {
    let mut moved = 0u64;
    let mut stored = 0;
    let mut current = None;
    loop {
        match rx.blocking_recv() {
            Some(Ok(Received::Head(head))) => {
                let next = wanted
                    .get(stored)
                    .map(|&(root, size)| BlobHead { root, size });
                if current.is_some() || next != Some(head) {
                    return Err(format!(
                        "blob {stored} the CI sent is not the one asked for"
                    ));
                }
                let receiving = store
                    .receive(head.root, head.size)
                    .map_err(|e| format!("blob {}: {e}", hex::encode(head.root)))?;
                current = Some(receiving);
            }
            Some(Ok(Received::Bytes(bytes))) => {
                moved += bytes.len() as u64;
                current
                    .as_mut()
                    .ok_or("the CI sent bytes outside a blob")?
                    .write(&bytes)?;
            }
            Some(Ok(Received::BlobEnd)) => {
                current
                    .take()
                    .ok_or("the CI ended a blob it did not begin")?
                    .finish()?;
                stored += 1;
            }
            Some(Ok(Received::End)) if stored == wanted.len() => return Ok(moved),
            Some(Ok(Received::End)) => {
                return Err(format!("the CI sent {stored} of {} blobs", wanted.len()));
            }
            Some(Err(message)) => return Err(message),
            None => return Err("the transfer ended early".to_owned()),
        }
    }
}

/// What `reader` yields up to its end marker, at most `limit` bytes.
fn read_to_limit(reader: &mut ChunkReader, limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let chunk = reader.fill_buf().map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            return Ok(out);
        }
        if out.len() + chunk.len() > limit {
            return Err(format!("more than {limit} bytes"));
        }
        out.extend_from_slice(chunk);
        let n = chunk.len();
        reader.consume(n);
    }
}

/// Writes the set that `reader` yields, byte for byte, into the file `dest`.
fn save(reader: &mut ChunkReader, dest: &Path) -> Result<(), String> {
    let mut file = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    loop {
        let chunk = reader.fill_buf().map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            return Ok(());
        }
        file.write_all(chunk)
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
        let n = chunk.len();
        reader.consume(n);
    }
}

/// Takes the zstd bytes of one blob after another, cuts them into chunks of
/// [`ARTIFACT_CHUNK`] bytes and hands them to the task that sends them. Where
/// a chunk ends is not the encoder's business: a write fills the chunk that
/// is open and starts the next.
struct ChunkWriter {
    tx: mpsc::Sender<Piece>,
    buf: Vec<u8>,
    /// Bytes written so far.
    total: u64,
    /// The most bytes it accepts.
    limit: u64,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<Piece>) -> ChunkWriter {
        ChunkWriter {
            tx,
            buf: Vec::with_capacity(ARTIFACT_CHUNK),
            total: 0,
            limit: MAX_ARTIFACT_BYTES,
        }
    }

    /// Hands on the chunk that is open and ends the blob.
    fn end_blob(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let last = std::mem::replace(&mut self.buf, Vec::with_capacity(ARTIFACT_CHUNK));
            self.send(Piece::Bytes(last))?;
        }
        self.send(Piece::BlobEnd)
    }

    fn send(&self, piece: Piece) -> io::Result<()> {
        self.tx
            .blocking_send(piece)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the transfer was abandoned"))
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.total += bytes.len() as u64;
        if self.total > self.limit {
            return Err(io::Error::other(format!("more than {} bytes", self.limit)));
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let take = rest.len().min(ARTIFACT_CHUNK - self.buf.len());
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == ARTIFACT_CHUNK {
                let full = std::mem::replace(&mut self.buf, Vec::with_capacity(ARTIFACT_CHUNK));
                self.send(Piece::Bytes(full))?;
            }
        }
        Ok(bytes.len())
    }

    /// The open chunk waits for more bytes until [`ChunkWriter::end_blob`].
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads the set from the chunks a receiving task passes on, on the calling
/// thread, which blocks until the next one arrives. It ends only at the end
/// marker: a task that failed, or went away before it, is an error, so a
/// truncated set is never taken for a complete one.
struct ChunkReader {
    rx: mpsc::Receiver<Result<Received, String>>,
    /// The chunk being read, and how much of it was.
    cur: Vec<u8>,
    pos: usize,
    /// The end marker arrived.
    ended: bool,
}

impl ChunkReader {
    fn new(rx: mpsc::Receiver<Result<Received, String>>) -> ChunkReader {
        ChunkReader {
            rx,
            cur: Vec::new(),
            pos: 0,
            ended: false,
        }
    }
}

impl BufRead for ChunkReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.pos == self.cur.len() {
            if self.ended {
                return Ok(&[]);
            }
            match self.rx.blocking_recv() {
                Some(Ok(Received::Bytes(bytes))) => {
                    self.cur = bytes;
                    self.pos = 0;
                }
                Some(Ok(Received::End)) => self.ended = true,
                Some(Ok(Received::Head(_) | Received::BlobEnd)) => {
                    return Err(io::Error::other("the CI sent a blob where none belongs"));
                }
                Some(Err(message)) => return Err(io::Error::other(message)),
                None => return Err(io::Error::other("the transfer ended early")),
            }
        }
        Ok(&self.cur[self.pos..])
    }

    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.cur.len());
    }
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes that differ along their length, so a chunk boundary that loses
    /// or repeats one shows.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn the_writer_cuts_a_blob_into_full_chunks_and_ends_it() {
        let (tx, mut rx) = mpsc::channel(8);
        let data = pattern(2 * ARTIFACT_CHUNK + 5);
        let mut writer = ChunkWriter::new(tx);
        // Writes that straddle the chunk boundaries.
        for part in data.chunks(300_000) {
            writer.write_all(part).unwrap();
        }
        writer.end_blob().unwrap();
        // An empty blob is only its end.
        writer.end_blob().unwrap();
        drop(writer);

        let (mut sizes, mut joined, mut ends) = (Vec::new(), Vec::new(), 0);
        while let Some(piece) = rx.blocking_recv() {
            match piece {
                Piece::Bytes(bytes) => {
                    sizes.push(bytes.len());
                    joined.extend_from_slice(&bytes);
                }
                Piece::BlobEnd => ends += 1,
                Piece::Head(_) | Piece::End => panic!("the writer sent a frame of its own"),
            }
        }
        assert_eq!(sizes, [ARTIFACT_CHUNK, ARTIFACT_CHUNK, 5]);
        assert_eq!(joined, data);
        assert_eq!(ends, 2);
    }

    #[test]
    fn the_writer_refuses_a_set_over_its_limit_and_one_nobody_takes() {
        let (tx, _rx) = mpsc::channel(QUEUE);
        let mut writer = ChunkWriter::new(tx);
        writer.limit = 10;
        writer.write_all(b"0123456789").unwrap();
        let err = writer.write_all(b"!").unwrap_err();
        assert!(err.to_string().contains("more than 10 bytes"), "{err}");

        // The receiving task failed and dropped its queue: the next chunk
        // that fills up finds nobody.
        let (tx, rx) = mpsc::channel(QUEUE);
        drop(rx);
        let mut writer = ChunkWriter::new(tx);
        let err = writer.write_all(&vec![0u8; ARTIFACT_CHUNK]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("zci-art-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// Bytes that do not compress, so a blob takes several chunks.
        fn noise(len: usize) -> Vec<u8> {
            let mut state = 0x9e37_79b9_7f4a_7c15u64;
            (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect()
        }

        /// Writes `bytes` to `path` and answers its head.
        fn file(path: &Path, bytes: &[u8]) -> BlobHead {
            std::fs::write(path, bytes).unwrap();
            let (size, root) = zeughaus_blobs::hash_file(path).unwrap();
            BlobHead { root, size }
        }

        /// What the producer compresses is what the consumer's cache holds,
        /// and the bytes it counts as moved are the compressed ones.
        #[test]
        fn compressed_blobs_arrive_whole_in_the_cache() {
            let base = scratch("blobs");
            let big = noise(2 * ARTIFACT_CHUNK + 12_345);
            let small = b"a small file".repeat(1000);
            let blobs = [
                (file(&base.join("big"), &big), base.join("big")),
                (file(&base.join("small"), &small), base.join("small")),
            ];
            let wanted: Vec<(Root, u64)> = blobs.iter().map(|(h, _)| (h.root, h.size)).collect();
            let (pieces_tx, mut pieces) = mpsc::channel(QUEUE);
            let (tx, rx) = mpsc::channel(QUEUE);
            let compressing = std::thread::spawn(move || compress_blobs(&blobs, pieces_tx));
            // Stands in for the wire: what the CI streams is what was sent.
            let carrying = std::thread::spawn(move || {
                let mut sent = 0u64;
                while let Some(piece) = pieces.blocking_recv() {
                    let received = match piece {
                        Piece::Head(head) => Received::Head(head),
                        Piece::Bytes(bytes) => {
                            sent += bytes.len() as u64;
                            Received::Bytes(bytes)
                        }
                        Piece::BlobEnd => Received::BlobEnd,
                        Piece::End => Received::End,
                    };
                    tx.blocking_send(Ok(received)).unwrap();
                }
                sent
            });
            let store = zeughaus_blobs::Store::open(&base.join("cache"), false).unwrap();

            let moved = store_blobs(&store, &wanted, rx).unwrap();

            compressing.join().unwrap().unwrap();
            assert_eq!(moved, carrying.join().unwrap());
            assert!(moved < (big.len() + small.len()) as u64);
            assert_eq!(std::fs::read(store.path(&wanted[0].0)).unwrap(), big);
            assert_eq!(std::fs::read(store.path(&wanted[1].0)).unwrap(), small);
            let _ = std::fs::remove_dir_all(&base);
        }

        /// The cache takes only the blobs asked for, in their order, and all
        /// of them.
        #[test]
        fn the_cache_refuses_other_blobs_and_too_few() {
            let base = scratch("refused-blobs");
            let store = zeughaus_blobs::Store::open(&base.join("cache"), false).unwrap();
            let asked = file(&base.join("asked"), b"asked for");
            let other = file(&base.join("other"), b"another");
            let mut zst = Vec::new();
            zeughaus_blobs::compress_file(&base.join("asked"), 3, &mut zst).unwrap();
            let wanted = [(asked.root, asked.size)];
            let run = |items: Vec<Received>| {
                let (tx, rx) = mpsc::channel(items.len() + 1);
                for item in items {
                    tx.blocking_send(Ok(item)).unwrap();
                }
                drop(tx);
                store_blobs(&store, &wanted, rx)
            };

            let err = run(vec![Received::Head(other)]).unwrap_err();
            assert!(err.contains("not the one asked for"), "{err}");
            let err = run(vec![Received::End]).unwrap_err();
            assert!(err.contains("sent 0 of 1 blobs"), "{err}");
            let err = run(vec![Received::Head(asked), Received::Bytes(zst)]).unwrap_err();
            assert!(err.contains("ended early"), "{err}");
            assert!(!store.has(&asked.root, asked.size));
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn what_the_ci_says_is_the_reason_the_bundle_did_not_arrive() {
            let base = scratch("refused");
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(b"part".to_vec())))
                .unwrap();
            tx.blocking_send(Err("the CI went away".to_owned()))
                .unwrap();
            let err = save(&mut ChunkReader::new(rx), &base.join("bundle")).unwrap_err();
            assert!(err.contains("the CI went away"), "{err}");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_bundle_is_written_chunk_by_chunk() {
            let base = scratch("bundle");
            let bundle = pattern(ARTIFACT_CHUNK + 7);
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(bundle[..ARTIFACT_CHUNK].to_vec())))
                .unwrap();
            tx.blocking_send(Ok(Received::Bytes(bundle[ARTIFACT_CHUNK..].to_vec())))
                .unwrap();
            tx.blocking_send(Ok(Received::End)).unwrap();
            let dest = base.join("bundle");
            save(&mut ChunkReader::new(rx), &dest).unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), bundle);

            // Without its end marker the file is no bundle.
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(bundle))).unwrap();
            drop(tx);
            assert!(save(&mut ChunkReader::new(rx), &dest).is_err());
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
