//! The `/artifacts` service: the CI relays a finished job's artifact set from
//! the runner that holds it to the runner of a job that needs it, without a
//! copy on this side.
//!
//! A consuming runner opens a `Download`; the CI asks the producing runner
//! for the set with [`ToRunner::Upload`] under a transfer number, and that
//! runner answers on an exchange of its own with `Upload` (the tar follows)
//! or `Unavailable`. The upload's bytes are streamed into the waiting
//! download's reply as they arrive. One exchange moves one tar; the request
//! is one frame ([`ArtifactRequest`]), an upload's bytes follow it, a
//! download's follow the reply frame ([`ArtifactReply`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use weida::{IncomingRequest, IncomingTransfer, Replier, TransferMeta};
use zeughaus_link::enlist::{
    ArtifactReply, ArtifactRequest, MAX_ARTIFACT_BYTES, ToRunner, parse_run_id, read_frame, run_id,
    write_frame,
};

use crate::config::{CiConfig, valid_name};
use crate::runners::Runners;
use crate::scheduler::{JobStatus, Pipeline, load_pipeline};
use crate::transport::principal_of;

/// Bytes moved per read or write.
const CHUNK: usize = 256 << 10;

/// How long a download waits for the producing runner to start its upload:
/// it packs the set into a tar first, which for a gigabyte takes a while.
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
    /// `size` bytes of tar follow on `body`; `done` hears how the relay went.
    Tar {
        size: u64,
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
            ArtifactRequest::Upload {
                run,
                transfer,
                size,
            } => {
                let relayed = self.upload(&runner, &run, transfer, size, body).await;
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
    /// has streamed it on.
    async fn upload(
        &self,
        runner: &str,
        run: &str,
        transfer: u64,
        size: u64,
        body: IncomingTransfer,
    ) -> Result<(), String> {
        if size > MAX_ARTIFACT_BYTES {
            return Err(format!("{size} bytes is more than {MAX_ARTIFACT_BYTES}"));
        }
        let waiting = self.take_waiting(runner, run, transfer)?;
        let (done, outcome) = oneshot::channel();
        if waiting.offer.send(Offer::Tar { size, body, done }).is_err() {
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
        let (size, mut body, done) = match offered {
            Some(Ok(Ok(Offer::Tar { size, body, done }))) => (size, body, done),
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
        let relayed = match write_frame(reply, &ArtifactReply::Sending { size }).await {
            Ok(()) => relay(&mut body, size, reply).await,
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
}

/// The runner that holds the set of `job` in `pipeline`, when `requester`
/// is the runner of a job that needs it and is assigned that job now.
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
            && assigned_to(&run_id(&pipeline.repo, pipeline.number, &j.def.name)).as_deref()
                == Some(requester)
    });
    if !consumer {
        return Err(format!("runner {requester} runs no job that needs {run}"));
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

/// Copies exactly `size` bytes of `body` to `out`; a body that ends short is
/// an error.
async fn relay(
    body: &mut (impl AsyncRead + Unpin),
    size: u64,
    out: &mut (impl AsyncWrite + Unpin),
) -> Result<(), String> {
    let mut buf = vec![0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let n = body
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("upload failed: {e}"))?;
        if n == 0 {
            return Err(format!("upload ended {left} bytes short"));
        }
        out.write_all(&buf[..n])
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        left -= n as u64;
    }
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

    #[tokio::test]
    async fn a_short_upload_is_an_error_and_a_full_one_is_exact() {
        let mut out = Vec::new();
        let mut body: &[u8] = b"0123456789";
        assert!(relay(&mut body, 12, &mut out).await.is_err());

        let mut out = Vec::new();
        let mut body: &[u8] = b"0123456789";
        relay(&mut body, 6, &mut out).await.expect("relay");
        assert_eq!(out, b"012345");
    }
}
