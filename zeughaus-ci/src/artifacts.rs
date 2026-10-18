//! The `/artifacts` service: the CI relays a finished job's artifact set from
//! the runner that holds it to the runner of a job that needs it, without a
//! copy on this side, and hands a runner the bundles of the manual run it was
//! assigned.
//!
//! A consuming runner opens a `Download`; the CI asks the producing runner
//! for the set with [`ToRunner::Upload`] under a transfer number, and that
//! runner answers on an exchange of its own with `Upload` (the chunks of the
//! set follow) or `Unavailable`. The upload's chunks are relayed into the
//! waiting download's reply as they arrive, so the producer packs and the
//! consumer unpacks while the set moves. One exchange moves one set; the
//! request is one frame ([`ArtifactRequest`]), an upload's chunks follow it,
//! a download's follow the reply frame ([`ArtifactReply`]), framed as
//! `zeughaus_link::enlist` describes. A `ManualBundle` is answered the same way,
//! from the file the `/manual` service stored.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use weida::{IncomingRequest, IncomingTransfer, Replier, TransferMeta};
use zeughaus_link::enlist::{
    ARTIFACT_CHUNK, ArtifactReply, ArtifactRequest, MAX_ARTIFACT_BYTES, ToRunner, parse_run_id,
    read_chunk, read_frame, run_id, write_chunk, write_end, write_frame,
};

use crate::config::{CiConfig, valid_name};
use crate::manual;
use crate::runners::Runners;
use crate::scheduler::{JobStatus, Pipeline, load_pipeline};
use crate::transport::principal_of;

/// How long a download waits for the producing runner to open its upload. The
/// producer packs while it sends, so this is the time to be told, to connect
/// and to pack the first chunk, not to pack the whole set.
const UPLOAD_WAIT: Duration = Duration::from_secs(600);

pub struct ArtifactService {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    runners: Arc<Runners>,
    next_transfer: AtomicU64,
    /// Downloads waiting for their upload, by transfer number.
    waiting: Mutex<HashMap<u64, Waiting>>,
}

/// A download that asked a producing runner for a set.
struct Waiting {
    run: String,
    runner: String,
    offer: oneshot::Sender<Offer>,
}

/// What the producing runner answered.
enum Offer {
    /// The set's chunks follow on `body`; `done` hears how the relay went.
    Tar {
        body: IncomingTransfer,
        done: oneshot::Sender<Result<(), String>>,
    },
    Unavailable(String),
}

impl ArtifactService {
    pub fn new(
        state_dir: PathBuf,
        config: Arc<CiConfig>,
        runners: Arc<Runners>,
    ) -> ArtifactService {
        ArtifactService {
            state_dir,
            config,
            runners,
            next_transfer: AtomicU64::new(0),
            waiting: Mutex::new(HashMap::new()),
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
            ArtifactRequest::Upload { run, transfer } => {
                let relayed = self.upload(&runner, &run, transfer, body).await;
                answer(&mut reply, &runner, &run, relayed).await
            }
            ArtifactRequest::Unavailable {
                run,
                transfer,
                message,
            } => {
                let passed = self
                    .take_waiting(&runner, &run, transfer)
                    .map(|waiting| drop(waiting.offer.send(Offer::Unavailable(message))));
                answer(&mut reply, &runner, &run, passed).await
            }
            ArtifactRequest::Download { run } => self.download(&runner, &run, &mut reply).await,
            ArtifactRequest::ManualBundle { run, index } => {
                self.manual_bundle(&runner, &run, index, &mut reply).await
            }
        };
        if let Err(e) = outcome {
            eprintln!("[ci] {runner}: artifact exchange failed: {e}");
        }
        let _ = reply.finish();
    }

    /// The waiting download `transfer`, when it asked `runner` for `run`.
    fn take_waiting(&self, runner: &str, run: &str, transfer: u64) -> Result<Waiting, String> {
        let mut waiting = lock(&self.waiting);
        match waiting.get(&transfer) {
            Some(w) if w.run == run && w.runner == runner => {}
            _ => {
                return Err(format!(
                    "no download waits for {run} as transfer {transfer}"
                ));
            }
        }
        Ok(waiting.remove(&transfer).expect("checked above"))
    }

    /// Hands the upload's body to its download and waits until the download
    /// has relayed it on.
    async fn upload(
        &self,
        runner: &str,
        run: &str,
        transfer: u64,
        body: IncomingTransfer,
    ) -> Result<(), String> {
        let waiting = self.take_waiting(runner, run, transfer)?;
        let (done, outcome) = oneshot::channel();
        if waiting.offer.send(Offer::Tar { body, done }).is_err() {
            return Err(format!("the download of {run} ended before the upload"));
        }
        outcome
            .await
            .unwrap_or_else(|_| Err(format!("the download of {run} ended")))
    }

    /// Asks the runner that holds `run`'s set for it and streams its upload
    /// into `reply`.
    async fn download(
        &self,
        requester: &str,
        run: &str,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        let producer = match self.producer_for(requester, run) {
            Ok(producer) => producer,
            Err(message) => return write_frame(reply, &ArtifactReply::Error { message }).await,
        };
        let transfer = self.next_transfer.fetch_add(1, Ordering::Relaxed);
        let (offer, offered) = oneshot::channel();
        lock(&self.waiting).insert(
            transfer,
            Waiting {
                run: run.to_owned(),
                runner: producer.clone(),
                offer,
            },
        );
        let asked = self.runners.send(
            &producer,
            ToRunner::Upload {
                run: run.to_owned(),
                transfer,
            },
        );
        let offered = if asked {
            Some(tokio::time::timeout(UPLOAD_WAIT, offered).await)
        } else {
            None
        };
        lock(&self.waiting).remove(&transfer);
        let refused = |message: String| ArtifactReply::Error { message };
        let (mut body, done) = match offered {
            Some(Ok(Ok(Offer::Tar { body, done }))) => (body, done),
            Some(Ok(Ok(Offer::Unavailable(message)))) => {
                return write_frame(reply, &refused(format!("runner {producer}: {message}"))).await;
            }
            None => {
                let message = format!("runner {producer}, which holds {run}, is not connected");
                return write_frame(reply, &refused(message)).await;
            }
            Some(Ok(Err(_))) => {
                let message = format!("the upload of {run} from runner {producer} was abandoned");
                return write_frame(reply, &refused(message)).await;
            }
            Some(Err(_)) => {
                let message = format!(
                    "runner {producer} did not send {run} within {} minutes",
                    UPLOAD_WAIT.as_secs() / 60
                );
                return write_frame(reply, &refused(message)).await;
            }
        };
        let relayed = match write_frame(reply, &ArtifactReply::Sending).await {
            Ok(()) => relay(&mut body, reply, MAX_ARTIFACT_BYTES).await,
            Err(e) => Err(e),
        };
        let _ = done.send(relayed.clone());
        relayed
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

/// The reply to an upload or an `Unavailable`.
async fn answer(
    reply: &mut weida::OutgoingTransfer,
    runner: &str,
    run: &str,
    outcome: Result<(), String>,
) -> Result<(), String> {
    match outcome {
        Ok(()) => write_frame(reply, &ArtifactReply::Done).await,
        Err(message) => {
            eprintln!("[ci] {runner}: artifacts of {run} not relayed: {message}");
            write_frame(reply, &ArtifactReply::Error { message }).await
        }
    }
}

/// Copies the chunks of `body` to `out` as they arrive, then the end marker.
/// A body that ends before its end marker, or whose chunks add up to more
/// than `limit` bytes, is an error, and `out` gets no end marker: whoever
/// reads it sees a broken transfer.
async fn relay(
    body: &mut (impl AsyncRead + Unpin),
    out: &mut (impl AsyncWrite + Unpin),
    limit: u64,
) -> Result<(), String> {
    let mut chunk = Vec::new();
    let mut total = 0u64;
    while read_chunk(body, &mut chunk)
        .await
        .map_err(|e| format!("upload failed: {e}"))?
    {
        total += chunk.len() as u64;
        if total > limit {
            return Err(format!("more than {limit} bytes"));
        }
        write_chunk(out, &chunk)
            .await
            .map_err(|e| format!("download failed: {e}"))?;
    }
    write_end(out)
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    out.flush()
        .await
        .map_err(|e| format!("download failed: {e}"))
}

/// A poisoned lock still holds a consistent map: every critical section is a
/// single insert, lookup or remove.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of a transfer of `parts` as chunks, with or without its end
    /// marker.
    async fn chunks(parts: &[&[u8]], end: bool) -> Vec<u8> {
        let mut wire = Vec::new();
        for part in parts {
            write_chunk(&mut wire, part).await.unwrap();
        }
        if end {
            write_end(&mut wire).await.unwrap();
        }
        wire
    }

    #[tokio::test]
    async fn a_relayed_set_keeps_its_chunks_and_its_end_marker() {
        let wire = chunks(&[b"0123", b"456789"], true).await;
        let mut body: &[u8] = &wire;
        let mut out = Vec::new();
        relay(&mut body, &mut out, 100).await.expect("relay");
        assert_eq!(out, wire);
    }

    #[tokio::test]
    async fn a_set_cut_short_or_over_the_limit_is_passed_on_without_its_marker() {
        // The upload stops before its end marker.
        let wire = chunks(&[b"0123", b"456789"], false).await;
        let mut body: &[u8] = &wire;
        let mut out = Vec::new();
        let err = relay(&mut body, &mut out, 100).await.unwrap_err();
        assert!(err.contains("before the end"), "{err}");
        assert_eq!(out, wire, "what arrived is passed on, nothing more");

        // The chunk that takes the total over the limit is not passed on.
        let wire = chunks(&[b"0123", b"456789"], true).await;
        let mut body: &[u8] = &wire;
        let mut out = Vec::new();
        let err = relay(&mut body, &mut out, 9).await.unwrap_err();
        assert!(err.contains("more than 9 bytes"), "{err}");
        assert_eq!(out, chunks(&[b"0123"], false).await);
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
        for status in ["pending", "waiting", "starting", "running", "frozen"] {
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
                ArtifactService::new(state.clone(), Arc::new(config), Arc::new(Runners::new()));
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
