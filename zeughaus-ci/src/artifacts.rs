//! The `/artifacts` service: the CI is the hub every artifact set passes
//! through on its way from the runner that made it to the runners of the
//! jobs that need it, and hands a runner the bundles of the manual run it
//! was assigned.
//!
//! The CI keeps a store of zstd-compressed blobs under `ci/blobs` and the
//! manifest of each set it holds under `ci/sets`. A consumer asks for a
//! set's `Manifest`; when the CI holds none yet, it asks the producing runner
//! with [`ToRunner::Upload`] under a transfer number, and that runner offers
//! the set's manifest on an exchange of its own (`Offer`, or `Unavailable`).
//! The CI answers with the roots its store lacks, and the producer sends only
//! those, compressed (`Blobs`). Once they are stored the set is recorded and
//! the consumer gets the manifest, then `Fetch`es the blobs its own cache
//! lacks. So a file crosses the producer's uplink once, and not again while
//! the store holds it. Framing is as `zeughaus_link::enlist` describes. A
//! `ManualBundle` is answered as chunks from the file the `/manual` service
//! stored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OnceCell, oneshot};
use weida::{IncomingRequest, IncomingTransfer, Replier, TransferMeta};
use zeughaus_blobs::{Retention, Root, Store, read_set, record_set};
use zeughaus_link::enlist::{
    ARTIFACT_CHUNK, ArtifactReply, ArtifactRequest, BlobHead, MAX_ARTIFACT_BYTES,
    MAX_MANIFEST_BYTES, MAX_SET_ENTRIES, SetEntry, SetManifest, ToRunner, decode_roots,
    encode_roots, parse_run_id, read_chunk, read_frame, run_id, write_chunk, write_end,
    write_frame,
};

use crate::config::{CiConfig, valid_name};
use crate::manual;
use crate::runners::Runners;
use crate::scheduler::{JobStatus, Pipeline, load_pipeline};
use crate::transport::principal_of;

/// Time until the producer offers the manifest, and again until it starts
/// sending the blobs the CI asked for. It hashes the set before it offers,
/// so this covers being told, connecting and hashing, not the transfer.
const UPLOAD_WAIT: Duration = Duration::from_secs(600);

/// How long the store keeps a set.
const SET_KEEP: Duration = Duration::from_secs(3 * 86400);

/// Bytes of compressed blobs the store may hold. sadala's disk is small and
/// holds a runner's cache too.
const STORE_BUDGET: u64 = 2 << 30;

/// A fill of one run's set, shared by every request for it.
type Fill = Arc<OnceCell<Result<(), String>>>;

pub struct ArtifactService {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    runners: Arc<Runners>,
    next_transfer: AtomicU64,
    store: Arc<Store>,
    sets: PathBuf,
    /// The fill in flight for a run, so concurrent consumers ask its
    /// producer once.
    fills: Mutex<HashMap<String, Fill>>,
    /// Fills waiting for their producer, by transfer number.
    waiting: Mutex<HashMap<u64, Waiting>>,
}

/// A fill that asked a producing runner for a set.
struct Waiting {
    run: String,
    runner: String,
    /// The offered manifest, once the producer offered it and the CI asked
    /// for `missing`.
    manifest: Option<SetManifest>,
    missing: Vec<(Root, u64)>,
    /// Told when the producer's offer asked for blobs; dropped when it asked
    /// for none.
    offered: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Sender<Result<(), String>>>,
}

impl Waiting {
    fn finish(&mut self, outcome: Result<(), String>) {
        if let Some(done) = self.done.take() {
            let _ = done.send(outcome);
        }
    }
}

impl ArtifactService {
    pub fn new(
        state_dir: PathBuf,
        config: Arc<CiConfig>,
        runners: Arc<Runners>,
    ) -> Result<ArtifactService, String> {
        let blobs = state_dir.join("ci/blobs");
        let store = Store::open(&blobs, true)
            .map_err(|e| format!("cannot open {}: {e}", blobs.display()))?;
        Ok(ArtifactService {
            sets: state_dir.join("ci/sets"),
            state_dir,
            config,
            runners,
            next_transfer: AtomicU64::new(0),
            store: Arc::new(store),
            fills: Mutex::new(HashMap::new()),
            waiting: Mutex::new(HashMap::new()),
        })
    }

    /// Keeps the store within [`SET_KEEP`] and [`STORE_BUDGET`], leaving
    /// alone what is younger than `grace`.
    pub fn sweep(&self, grace: Duration) {
        let retention = Retention {
            keep: SET_KEEP,
            budget: STORE_BUDGET,
            grace,
        };
        let swept = zeughaus_blobs::sweep(&self.store, &self.sets, &retention);
        if swept.sets > 0 || swept.blobs > 0 {
            eprintln!(
                "[ci] artifact store: removed {} sets, {} blobs, {} MB",
                swept.sets,
                swept.blobs,
                swept.bytes >> 20
            );
        }
    }

    /// Accepts exchanges until the replier goes away, which for this process
    /// means never.
    pub async fn accept(self: Arc<Self>, replier: Replier) {
        loop {
            match replier.accept().await {
                Ok(request) => {
                    let service = Arc::clone(&self);
                    tokio::spawn(async move { service.exchange(request).await });
                }
                Err(e) => {
                    eprintln!("[ci] stopped accepting artifacts: {e}");
                    return;
                }
            }
        }
    }

    async fn exchange(&self, mut request: IncomingRequest) {
        let runner = principal_of(request.meta()).and_then(|principal| {
            self.config
                .runner_of_fingerprint(&principal)
                .map(str::to_owned)
        });
        let Some(runner) = runner else {
            eprintln!("[ci] refused an artifact exchange from a peer that is no runner");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        };
        let mut body = request.take_body();
        let first = match read_frame::<_, ArtifactRequest>(&mut body).await {
            Ok(Some(first)) => first,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[ci] {runner}: unreadable artifact request: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] {runner}: cannot open the reply half: {e}");
                return;
            }
        };
        let outcome = match first {
            ArtifactRequest::Offer { run, transfer } => {
                self.offer(&runner, &run, transfer, body, &mut reply).await
            }
            ArtifactRequest::Blobs { run, transfer } => {
                self.blobs(&runner, &run, transfer, body, &mut reply).await
            }
            ArtifactRequest::Unavailable {
                run,
                transfer,
                message,
            } => {
                let passed = self
                    .take_waiting(&runner, &run, transfer)
                    .map(|mut waiting| {
                        waiting.finish(Err(format!("runner {runner}: {message}")));
                    });
                answer(&mut reply, &runner, &run, passed).await
            }
            ArtifactRequest::Manifest { run } => self.manifest(&runner, &run, &mut reply).await,
            ArtifactRequest::Fetch { run } => self.fetch(&runner, &run, body, &mut reply).await,
            ArtifactRequest::ManualBundle { run, index } => {
                self.manual_bundle(&runner, &run, index, &mut reply).await
            }
        };
        if let Err(e) = outcome {
            eprintln!("[ci] {runner}: artifact exchange failed: {e}");
        }
        let _ = reply.finish();
    }

    /// The fill `transfer`, when it asked `runner` for `run`.
    fn take_waiting(&self, runner: &str, run: &str, transfer: u64) -> Result<Waiting, String> {
        let mut waiting = lock(&self.waiting);
        match waiting.get(&transfer) {
            Some(w) if w.run == run && w.runner == runner => {}
            _ => return Err(format!("no fill waits for {run} as transfer {transfer}")),
        }
        Ok(waiting.remove(&transfer).expect("checked above"))
    }

    /// The manifest of `run`'s set, filled from its producer when the store
    /// does not hold it yet. Concurrent callers share one fill; a failed fill
    /// is tried again by the next caller.
    async fn ensure_stored(&self, run: &str, producer: &str) -> Result<SetManifest, String> {
        if let Some(manifest) = read_set(&self.sets, run)? {
            return Ok(manifest);
        }
        let cell = Arc::clone(lock(&self.fills).entry(run.to_owned()).or_default());
        let filled = cell.get_or_init(|| self.fill(run, producer)).await.clone();
        {
            let mut fills = lock(&self.fills);
            if fills.get(run).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
                fills.remove(run);
            }
        }
        filled?;
        read_set(&self.sets, run)?.ok_or_else(|| format!("the set of {run} is gone"))
    }

    /// Asks `producer` for the set of `run` and waits until it is stored.
    async fn fill(&self, run: &str, producer: &str) -> Result<(), String> {
        let transfer = self.next_transfer.fetch_add(1, Ordering::Relaxed);
        let (offered, offered_rx) = oneshot::channel();
        let (done, done_rx) = oneshot::channel();
        lock(&self.waiting).insert(
            transfer,
            Waiting {
                run: run.to_owned(),
                runner: producer.to_owned(),
                manifest: None,
                missing: Vec::new(),
                offered: Some(offered),
                done: Some(done),
            },
        );
        let filled = self
            .await_fill(run, producer, transfer, offered_rx, done_rx)
            .await;
        lock(&self.waiting).remove(&transfer);
        filled
    }

    async fn await_fill(
        &self,
        run: &str,
        producer: &str,
        transfer: u64,
        offered: oneshot::Receiver<()>,
        mut done: oneshot::Receiver<Result<(), String>>,
    ) -> Result<(), String> {
        let abandoned = || format!("the upload of {run} from runner {producer} was abandoned");
        let asked = self.runners.send(
            producer,
            ToRunner::Upload {
                run: run.to_owned(),
                transfer,
            },
        );
        if !asked {
            return Err(format!(
                "runner {producer}, which holds {run}, is not connected"
            ));
        }
        let minutes = UPLOAD_WAIT.as_secs() / 60;
        // Closed: the offer needed no blobs, or the fill failed; `done` says
        // which.
        if tokio::time::timeout(UPLOAD_WAIT, offered).await.is_err() {
            return Err(format!(
                "runner {producer} did not offer {run} within {minutes} minutes"
            ));
        }
        if let Ok(outcome) = tokio::time::timeout(UPLOAD_WAIT, &mut done).await {
            return outcome.unwrap_or_else(|_| Err(abandoned()));
        }
        // Still waiting here means the blobs never came; once their exchange
        // took the entry, it runs as long as its stream lives.
        if lock(&self.waiting).remove(&transfer).is_some() {
            return Err(format!(
                "runner {producer} offered {run} but sent no blobs within {minutes} minutes"
            ));
        }
        done.await.unwrap_or_else(|_| Err(abandoned()))
    }

    /// The producer offers the manifest of the set `transfer` asked for; the
    /// reply names the blobs the store lacks.
    async fn offer(
        &self,
        runner: &str,
        run: &str,
        transfer: u64,
        mut body: IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        let expected = matches!(
            lock(&self.waiting).get(&transfer),
            Some(w) if w.run == run && w.runner == runner && w.offered.is_some()
        );
        if !expected {
            let message = format!("no fill waits for an offer of {run} as transfer {transfer}");
            return write_frame(reply, &ArtifactReply::Error { message }).await;
        }
        match self.read_offer(run, &mut body).await {
            Ok((manifest, missing)) if missing.is_empty() => {
                let recorded = record(&self.sets, run, manifest).await;
                if let Ok(mut waiting) = self.take_waiting(runner, run, transfer) {
                    waiting.finish(recorded.clone());
                }
                match recorded {
                    Ok(()) => {
                        write_frame(reply, &ArtifactReply::Sending).await?;
                        write_end(reply).await?;
                        flush(reply).await
                    }
                    Err(message) => write_frame(reply, &ArtifactReply::Error { message }).await,
                }
            }
            Ok((manifest, missing)) => {
                let roots: Vec<Root> = missing.iter().map(|(root, _)| *root).collect();
                let asked = match lock(&self.waiting).get_mut(&transfer) {
                    Some(entry) => {
                        entry.manifest = Some(manifest);
                        entry.missing = missing;
                        if let Some(offered) = entry.offered.take() {
                            let _ = offered.send(());
                        }
                        true
                    }
                    None => false,
                };
                if !asked {
                    let message = format!("the fill of {run} gave up waiting");
                    return write_frame(reply, &ArtifactReply::Error { message }).await;
                }
                write_frame(reply, &ArtifactReply::Sending).await?;
                send_bytes(reply, &encode_roots(&roots)).await?;
                write_end(reply).await?;
                flush(reply).await
            }
            Err(message) => {
                eprintln!("[ci] {runner}: offer of {run} refused: {message}");
                if let Ok(mut waiting) = self.take_waiting(runner, run, transfer) {
                    waiting.finish(Err(message.clone()));
                }
                write_frame(reply, &ArtifactReply::Error { message }).await
            }
        }
    }

    /// The offered manifest and the blobs of it the store lacks.
    async fn read_offer(
        &self,
        run: &str,
        body: &mut IncomingTransfer,
    ) -> Result<(SetManifest, Vec<(Root, u64)>), String> {
        let bytes = read_body(body, MAX_MANIFEST_BYTES)
            .await
            .map_err(|e| format!("the manifest of {run}: {e}"))?;
        let manifest =
            SetManifest::decode(&bytes).map_err(|e| format!("the manifest of {run}: {e}"))?;
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let missing = zeughaus_blobs::missing(&manifest, &store);
            (manifest, missing)
        })
        .await
        .map_err(|e| format!("the store failed: {e}"))
    }

    /// The producer sends the blobs its offer was asked for.
    async fn blobs(
        &self,
        runner: &str,
        run: &str,
        transfer: u64,
        mut body: IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        let taken = {
            let mut waiting = lock(&self.waiting);
            match waiting.get(&transfer) {
                Some(w) if w.run == run && w.runner == runner && w.manifest.is_some() => {
                    waiting.remove(&transfer)
                }
                _ => None,
            }
        };
        let Some(mut waiting) = taken else {
            let message = format!("no offer of {run} waits for its blobs as transfer {transfer}");
            return write_frame(reply, &ArtifactReply::Error { message }).await;
        };
        let mut stored = self.receive_blobs(run, &waiting.missing, &mut body).await;
        if stored.is_ok() {
            let manifest = waiting.manifest.take().expect("checked above");
            stored = record(&self.sets, run, manifest).await;
        }
        waiting.finish(stored.clone());
        answer(reply, runner, run, stored).await
    }

    /// Reads each blob of `missing` from `body` into the store, in order,
    /// and then the end of the body.
    async fn receive_blobs(
        &self,
        run: &str,
        missing: &[(Root, u64)],
        body: &mut IncomingTransfer,
    ) -> Result<(), String> {
        let joined = |e: tokio::task::JoinError| format!("the store failed: {e}");
        let mut chunk = Vec::new();
        let mut total = 0u64;
        for (i, &(root, size)) in missing.iter().enumerate() {
            let head = read_frame::<_, BlobHead>(body)
                .await?
                .ok_or_else(|| format!("the upload of {run} ended after {i} blobs"))?;
            if head != (BlobHead { root, size }) {
                return Err(format!("blob {i} of {run} is not the one asked for"));
            }
            let store = Arc::clone(&self.store);
            let mut receiving = tokio::task::spawn_blocking(move || store.receive(root, size))
                .await
                .map_err(joined)?
                .map_err(|e| format!("blob {}: {e}", hex::encode(root)))?;
            while read_chunk(body, &mut chunk).await? {
                total += chunk.len() as u64;
                if total > MAX_ARTIFACT_BYTES {
                    return Err(format!("more than {MAX_ARTIFACT_BYTES} bytes"));
                }
                let bytes = std::mem::take(&mut chunk);
                (receiving, chunk) = tokio::task::spawn_blocking(move || {
                    receiving.write(&bytes).map(|()| (receiving, bytes))
                })
                .await
                .map_err(joined)??;
            }
            tokio::task::spawn_blocking(move || receiving.finish())
                .await
                .map_err(joined)??;
        }
        match read_frame::<_, BlobHead>(body).await? {
            None => Ok(()),
            Some(_) => Err(format!(
                "the upload of {run} sent more blobs than asked for"
            )),
        }
    }

    /// The manifest of `run`'s set, for a runner whose job needs it.
    async fn manifest(
        &self,
        requester: &str,
        run: &str,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        let producer = match self.producer_for(requester, run) {
            Ok(producer) => producer,
            Err(message) => return write_frame(reply, &ArtifactReply::Error { message }).await,
        };
        let manifest = match self.ensure_stored(run, &producer).await {
            Ok(manifest) => manifest,
            Err(message) => return write_frame(reply, &ArtifactReply::Error { message }).await,
        };
        write_frame(reply, &ArtifactReply::Sending).await?;
        send_bytes(reply, &manifest.encode()).await?;
        write_end(reply).await?;
        flush(reply).await
    }

    /// The blobs of `run`'s set the requester names, compressed as stored.
    async fn fetch(
        &self,
        requester: &str,
        run: &str,
        mut body: IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        let wanted = match self.wanted(requester, run, &mut body).await {
            Ok(wanted) => wanted,
            Err(message) => return write_frame(reply, &ArtifactReply::Error { message }).await,
        };
        write_frame(reply, &ArtifactReply::Sending).await?;
        let mut chunk = vec![0u8; ARTIFACT_CHUNK];
        for head in wanted {
            let path = self.store.path(&head.root);
            write_frame(reply, &head).await?;
            // Without its end marker the consumer sees a broken transfer.
            let mut file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| format!("{}: {e}", path.display()))?;
            loop {
                let n = file
                    .read(&mut chunk)
                    .await
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if n == 0 {
                    break;
                }
                write_chunk(reply, &chunk[..n]).await?;
            }
            write_end(reply).await?;
        }
        flush(reply).await
    }

    /// The blobs a `Fetch` names, each a file of the set the CI holds.
    async fn wanted(
        &self,
        requester: &str,
        run: &str,
        body: &mut IncomingTransfer,
    ) -> Result<Vec<BlobHead>, String> {
        self.producer_for(requester, run)?;
        let manifest = read_set(&self.sets, run)?
            .ok_or_else(|| format!("the CI holds no set of {run}; ask for its manifest first"))?;
        let bytes = read_body(body, 32 * MAX_SET_ENTRIES).await?;
        let sizes: HashMap<Root, u64> = manifest
            .entries
            .iter()
            .filter_map(|entry| match entry {
                SetEntry::File { root, size, .. } => Some((*root, *size)),
                SetEntry::Dir { .. } => None,
            })
            .collect();
        decode_roots(&bytes)?
            .into_iter()
            .map(|root| match sizes.get(&root) {
                Some(&size) => Ok(BlobHead { root, size }),
                None => Err(format!("{} is no file of {run}", hex::encode(root))),
            })
            .collect()
    }

    /// The runner holding `run`'s set, when `requester` runs a job that needs
    /// it. The name came from a peer and is joined into a path, so it is
    /// checked before and against the records after.
    fn producer_for(&self, requester: &str, run: &str) -> Result<String, String> {
        let (repo, number, job) = parse_run_id(run).ok_or_else(|| format!("{run} is not a run"))?;
        if !valid_name(&repo) || !valid_name(&job) {
            return Err(format!("{run} is not a run"));
        }
        let pipeline = load_pipeline(&self.state_dir, &repo, number)
            .map_err(|_| format!("{run} is not a known run"))?;
        producer_for(&pipeline, &job, requester, |run| {
            self.runners.assigned_to(run)
        })
    }

    /// Streams bundle `index` of the manual run `run` into `reply` as chunks.
    async fn manual_bundle(
        &self,
        requester: &str,
        run: &str,
        index: u32,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        let path = match self.bundle_for(requester, run, index) {
            Ok(path) => path,
            Err(message) => return write_frame(reply, &ArtifactReply::Error { message }).await,
        };
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) => {
                let message = format!("cannot read bundle {index} of {run}: {e}");
                return write_frame(reply, &ArtifactReply::Error { message }).await;
            }
        };
        write_frame(reply, &ArtifactReply::Sending).await?;
        let mut chunk = vec![0u8; ARTIFACT_CHUNK];
        let mut total = 0u64;
        loop {
            let n = file
                .read(&mut chunk)
                .await
                .map_err(|e| format!("cannot read bundle {index} of {run}: {e}"))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_ARTIFACT_BYTES {
                return Err(format!("more than {MAX_ARTIFACT_BYTES} bytes"));
            }
            write_chunk(reply, &chunk[..n]).await?;
        }
        write_end(reply).await?;
        reply
            .flush()
            .await
            .map_err(|e| format!("download failed: {e}"))
    }

    /// The file of bundle `index` of the manual run `run`, when `requester` is
    /// the runner it is assigned to. The name came from a peer and is joined
    /// into a path, so it is checked before and against the records after.
    fn bundle_for(&self, requester: &str, run: &str, index: u32) -> Result<PathBuf, String> {
        let (repo, number, job) = parse_run_id(run).ok_or_else(|| format!("{run} is not a run"))?;
        if !valid_name(&repo) || !valid_name(&job) {
            return Err(format!("{run} is not a run"));
        }
        let pipeline = load_pipeline(&self.state_dir, &repo, number)
            .map_err(|_| format!("{run} is not a known run"))?;
        let id =
            manual::upload_of(&pipeline).ok_or_else(|| format!("{run} is not a manual run"))?;
        if self.runners.assigned_to(run).as_deref() != Some(requester) {
            return Err(format!("runner {requester} is not assigned {run}"));
        }
        let upload = manual::read_upload(&self.state_dir, id)
            .map_err(|_| format!("the upload of {run} is gone"))?;
        if index as usize >= upload.dirs.len() {
            return Err(format!(
                "{run} has {} bundles, not {index}",
                upload.dirs.len()
            ));
        }
        Ok(manual::bundle_path(&self.state_dir, id, index))
    }
}

/// The runner that holds the set of `job` in `pipeline`, when a job that
/// needs it is assigned to `requester` now, or is placed on it and has not
/// ended: the runner fetches the set early, before that job is assigned.
fn producer_for(
    pipeline: &Pipeline,
    job: &str,
    requester: &str,
    assigned_to: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let run = run_id(&pipeline.repo, pipeline.number, job);
    let record = pipeline
        .jobs
        .iter()
        .find(|j| j.def.name == job)
        .ok_or_else(|| format!("{run} is not a known run"))?;
    let producer = match &record.runner {
        Some(runner) if record.status == JobStatus::Succeeded && record.artifacts => runner,
        _ => return Err(format!("{run} left no artifacts")),
    };
    let consumer = pipeline.jobs.iter().any(|j| {
        j.def.needs.iter().any(|need| need == job)
            && (assigned_to(&run_id(&pipeline.repo, pipeline.number, &j.def.name)).as_deref()
                == Some(requester)
                || (!j.status.is_terminal() && j.runner.as_deref() == Some(requester)))
    });
    if !consumer {
        return Err(format!(
            "runner {requester} has no unfinished job that needs {run}"
        ));
    }
    Ok(producer.clone())
}

/// The reply to a `Blobs` or an `Unavailable`.
async fn answer(
    reply: &mut weida::OutgoingTransfer,
    runner: &str,
    run: &str,
    outcome: Result<(), String>,
) -> Result<(), String> {
    match outcome {
        Ok(()) => write_frame(reply, &ArtifactReply::Done).await,
        Err(message) => {
            eprintln!("[ci] {runner}: artifacts of {run} not stored: {message}");
            write_frame(reply, &ArtifactReply::Error { message }).await
        }
    }
}

/// The chunks of `body` up to its end marker, at most `limit` bytes of them.
async fn read_body(body: &mut (impl AsyncRead + Unpin), limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chunk = Vec::new();
    while read_chunk(body, &mut chunk).await? {
        if out.len() + chunk.len() > limit {
            return Err(format!("more than {limit} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `bytes` as chunks, without the end marker.
async fn send_bytes(reply: &mut weida::OutgoingTransfer, bytes: &[u8]) -> Result<(), String> {
    for chunk in bytes.chunks(ARTIFACT_CHUNK) {
        write_chunk(reply, chunk).await?;
    }
    Ok(())
}

async fn flush(reply: &mut weida::OutgoingTransfer) -> Result<(), String> {
    reply
        .flush()
        .await
        .map_err(|e| format!("the reply failed: {e}"))
}

/// Records `manifest` as the set of `run`, off the async threads.
async fn record(sets: &Path, run: &str, manifest: SetManifest) -> Result<(), String> {
    let (sets, run) = (sets.to_owned(), run.to_owned());
    tokio::task::spawn_blocking(move || record_set(&sets, &run, &manifest))
        .await
        .map_err(|e| format!("the store failed: {e}"))?
}

/// A poisoned lock still holds a consistent map: every critical section is a
/// single insert, lookup or remove.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_body_cut_short_or_over_its_limit_is_refused() {
        let mut wire = Vec::new();
        write_chunk(&mut wire, b"0123").await.unwrap();
        write_chunk(&mut wire, b"456789").await.unwrap();
        let cut = wire.clone();
        write_end(&mut wire).await.unwrap();

        assert_eq!(read_body(&mut &wire[..], 10).await.unwrap(), b"0123456789");
        let err = read_body(&mut &wire[..], 9).await.unwrap_err();
        assert!(err.contains("more than 9 bytes"), "{err}");
        let err = read_body(&mut &cut[..], 100).await.unwrap_err();
        assert!(err.contains("before the end"), "{err}");
    }

    /// Pipeline `demo` #1: `a` ran on `x` and left artifacts; `b` needs it
    /// and is placed on `y`, in `status`.
    fn needs_a(status: &str) -> Pipeline {
        let def = |name: &str, needs: &[&str]| {
            serde_json::json!({
                "name": name, "file": format!("{name}.sh"), "on": [], "needs": needs,
                "image": null, "machine": null, "secrets": [],
                "when_busy": "wait", "env": {}, "timeout_minutes": 5, "interpreter": "sh",
            })
        };
        serde_json::from_value(serde_json::json!({
            "repo": "demo", "number": 1, "key": "", "sha": null, "status": "running",
            "created": 0,
            "event": {
                "repo": "demo", "kind": "push", "git_ref": "main", "sha": null, "cron": null,
                "actor": "a", "delivery": null, "received": 0,
            },
            "jobs": [
                { "def": def("a", &[]), "status": "succeeded", "runner": "x", "artifacts": true },
                { "def": def("b", &["a"]), "status": status, "runner": "y" },
            ],
        }))
        .unwrap()
    }

    /// A set goes to the runner of a job that needs it and has not ended,
    /// handed over or not (that runner fetches it early), and to no other.
    #[test]
    fn a_set_goes_to_the_runner_of_a_job_that_needs_it_and_has_not_ended() {
        let unassigned = |_: &str| None;
        for status in ["pending", "waiting", "starting", "running"] {
            let pipeline = needs_a(status);
            assert_eq!(
                producer_for(&pipeline, "a", "y", unassigned),
                Ok("x".to_owned()),
                "{status}"
            );
            let err = producer_for(&pipeline, "a", "z", unassigned).unwrap_err();
            assert!(
                err.contains("no unfinished job that needs"),
                "{status}: {err}"
            );
        }
        for status in ["succeeded", "failed", "skipped"] {
            let pipeline = needs_a(status);
            assert!(
                producer_for(&pipeline, "a", "y", unassigned).is_err(),
                "a job that ended asks for nothing ({status})"
            );
        }

        // A job handed to a runner is served as before, whichever runner it
        // was placed on.
        let handed_to_z = |run: &str| (run == "demo.1.b").then(|| "z".to_owned());
        assert_eq!(
            producer_for(&needs_a("starting"), "a", "z", handed_to_z),
            Ok("x".to_owned())
        );
        // The set must exist: a job that left none has nothing to send.
        let mut none = needs_a("pending");
        none.jobs[0].artifacts = false;
        assert!(producer_for(&none, "a", "y", unassigned).is_err());
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        const FP: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";

        const UPLOAD: &str = "0123456789abcdef";

        /// A service over a state directory that holds pipeline `demo` #3 (a
        /// manual run, or a push) with an upload of two bundles.
        fn service_with(kind: &str) -> (ArtifactService, PathBuf) {
            use zeughaus_link::enlist::UploadDir;
            use zeughaus_link::manual::RunUpload;

            let state =
                std::env::temp_dir().join(format!("zci-bundles-{kind}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&state);
            let record = serde_json::json!({
                "repo": "demo", "number": 3, "key": "", "sha": null, "status": "running",
                "created": 0, "jobs": [],
                "event": {
                    "repo": "demo", "kind": kind, "git_ref": "main", "sha": null, "cron": null,
                    "actor": "a", "delivery": null, "upload": UPLOAD, "received": 0,
                },
            });
            crate::files::write_json(&state.join("ci/pipelines/demo/3.json"), &record).unwrap();
            let dir = |name: &str| UploadDir {
                dir: name.to_owned(),
                sha: "a".repeat(40),
            };
            let upload = RunUpload {
                origin: "git@example.org:me/demo.git".to_owned(),
                actor: "a".to_owned(),
                summary: "true".to_owned(),
                jobs: vec!["build".to_owned()],
                dirs: vec![dir("demo"), dir("lib")],
                sizes: vec![5, 3],
            };
            std::fs::create_dir_all(manual::upload_dir(&state, UPLOAD)).unwrap();
            std::fs::write(manual::bundle_path(&state, UPLOAD, 0), b"first").unwrap();
            std::fs::write(manual::bundle_path(&state, UPLOAD, 1), b"lib").unwrap();
            manual::write_upload(&state, UPLOAD, &upload).unwrap();
            let config = CiConfig::parse(&format!(
                "default_runner = \"x\"\n[runners.x]\nfingerprint = \"{FP}\"\n"
            ))
            .unwrap();
            let service =
                ArtifactService::new(state.clone(), Arc::new(config), Arc::new(Runners::new()))
                    .unwrap();
            (service, state)
        }

        /// A runner gets the bundles of the manual run it is assigned, and of no
        /// other run: not another runner's, not a push's, not a path.
        #[test]
        fn a_manual_bundle_goes_only_to_the_runner_assigned_the_run() {
            let (service, state) = service_with("manual");
            let run = "demo.3.build";
            let path = |index| manual::bundle_path(&state, UPLOAD, index);

            assert!(service.bundle_for("x", run, 0).is_err(), "not assigned yet");
            service.runners.assign(run, "x");
            assert_eq!(service.bundle_for("x", run, 0), Ok(path(0)));
            assert_eq!(service.bundle_for("x", run, 1), Ok(path(1)));
            assert!(service.bundle_for("x", run, 2).is_err(), "no third bundle");
            assert!(service.bundle_for("y", run, 0).is_err(), "another runner");
            for bad in [
                "demo.4.build",
                "../demo.3.build",
                "demo.3.../build",
                "demo",
                "",
            ] {
                assert!(service.bundle_for("x", bad, 0).is_err(), "{bad:?}");
            }
            service.runners.unassign(run);
            assert!(
                service.bundle_for("x", run, 0).is_err(),
                "no longer assigned"
            );
            let _ = std::fs::remove_dir_all(&state);

            // The same upload named by a pipeline that is no manual run.
            let (service, state) = service_with("push");
            service.runners.assign(run, "x");
            assert!(service.bundle_for("x", run, 0).is_err());
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}
