//! The `/enlist` service: a runner dials in, is checked against `ci.toml`,
//! and then holds one long exchange on which it sends its status and its
//! runs' output and receives assignments.
//!
//! The run output is written here, not by the scheduler: it is the bulk of
//! the traffic and needs no decision. Everything else is handed to the
//! scheduler, which owns the records.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use tokio::sync::mpsc::unbounded_channel;
use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_link::enlist::{
    ENLIST_MAJOR, FromRunner, MAX_OUTPUT_CHUNK, RunReport, ToRunner, parse_run_id, read_frame,
    write_frame,
};

use crate::config::{CiConfig, valid_name};
use crate::now_secs;
use crate::runners::{Connected, Runners};
use crate::scheduler::{Msg, transcript_dir};
use crate::transport::principal_of;

/// How long a dialled-in peer has to say who it is.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

pub struct EnlistService {
    state_dir: std::path::PathBuf,
    config: Arc<CiConfig>,
    runners: Arc<Runners>,
    scheduler: Sender<Msg>,
}

impl EnlistService {
    pub fn new(
        state_dir: std::path::PathBuf,
        config: Arc<CiConfig>,
        runners: Arc<Runners>,
        scheduler: Sender<Msg>,
    ) -> EnlistService {
        EnlistService {
            state_dir,
            config,
            runners,
            scheduler,
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
                    eprintln!("[ci] stopped accepting runners: {e}");
                    return;
                }
            }
        }
    }

    /// How much of each reported run's output this side holds.
    fn offsets(&self, runs: &[RunReport]) -> Vec<(String, u64)> {
        runs.iter()
            .filter_map(|report| {
                let wal = wal_path(&self.state_dir, &report.run)?;
                let held = std::fs::metadata(wal).map_or(0, |m| m.len());
                Some((report.run.clone(), held))
            })
            .collect()
    }

    /// One runner's exchange, from its hello to its end.
    async fn exchange(&self, mut request: IncomingRequest) {
        let principal = principal_of(request.meta());
        let mut body = request.take_body();
        let canceled = request.canceled();
        let first = tokio::time::timeout(HELLO_TIMEOUT, read_frame::<_, FromRunner>(&mut body))
            .await
            .map_err(|_| "no hello".to_owned())
            .and_then(|read| read);
        let hello = match first {
            Ok(Some(hello)) => hello,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[ci] unreadable hello: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] cannot open the reply half to a runner: {e}");
                return;
            }
        };

        let FromRunner::Hello {
            major,
            name,
            offer,
            runs,
        } = hello
        else {
            refuse(reply, "the first frame must be a hello").await;
            return;
        };
        let known = principal
            .as_deref()
            .and_then(|principal| self.config.runner_of_fingerprint(principal));
        if known != Some(name.as_str()) {
            eprintln!("[ci] refused {name:?}: its key is not the one ci.toml pins for it");
            refuse(reply, "this key is not the runner's in ci.toml").await;
            return;
        }
        if major != ENLIST_MAJOR {
            refuse(reply, &format!("protocol {major} is not {ENLIST_MAJOR}")).await;
            return;
        }

        let welcome = ToRunner::Welcome {
            major: ENLIST_MAJOR,
            offsets: self.offsets(&runs),
        };
        if let Err(e) = write_frame(&mut reply, &welcome).await {
            eprintln!("[ci] cannot welcome {name}: {e}");
            return;
        }
        let (tx, mut rx) = unbounded_channel::<ToRunner>();
        let id = self.runners.insert(
            &name,
            Connected {
                tx,
                offer,
                busy: None,
                machines: Default::default(),
                since: now_secs(),
            },
        );
        eprintln!("[ci] runner {name} enlisted");
        let _ = self.scheduler.send(Msg::RunnerHello {
            name: name.clone(),
            runs,
        });

        let mut reader = tokio::spawn(read_runner(
            body,
            name.clone(),
            self.state_dir.clone(),
            Arc::clone(&self.runners),
            self.scheduler.clone(),
        ));
        let mut canceled = std::pin::pin!(canceled);
        let cause = loop {
            tokio::select! {
                () = &mut canceled => break "the runner closed the exchange".to_owned(),
                ended = &mut reader => {
                    break match ended {
                        Ok(Ok(())) => "the runner ended the exchange".to_owned(),
                        Ok(Err(e)) => e,
                        Err(e) => format!("the reader failed: {e}"),
                    };
                }
                message = rx.recv() => {
                    // Closed: a newer connection of this runner replaced this one.
                    let Some(message) = message else {
                        break "replaced by a newer connection".to_owned();
                    };
                    if let Err(e) = write_frame(&mut reply, &message).await {
                        break e;
                    }
                }
            }
        };
        reader.abort();
        let _ = reply.finish();
        eprintln!("[ci] runner {name} gone: {cause}");
        if self.runners.remove(&name, id) {
            let _ = self.scheduler.send(Msg::RunnerGone { name });
        }
    }
}

async fn refuse(mut reply: weida::OutgoingTransfer, message: &str) {
    let refused = ToRunner::Refused {
        message: message.to_owned(),
    };
    let _ = write_frame(&mut reply, &refused).await;
    let _ = reply.finish();
}

/// Reads a runner's frames until its end: output is stored, everything else
/// goes to the scheduler. An `Err` ends the exchange.
async fn read_runner(
    mut body: weida::IncomingTransfer,
    name: String,
    state_dir: std::path::PathBuf,
    runners: Arc<Runners>,
    scheduler: Sender<Msg>,
) -> Result<(), String> {
    loop {
        let Some(message) = read_frame::<_, FromRunner>(&mut body).await? else {
            return Ok(());
        };
        match message {
            FromRunner::Hello { .. } => return Err("a second hello".to_owned()),
            FromRunner::Output { run, offset, bytes } => {
                if runners.assigned_to(&run).as_deref() != Some(name.as_str()) {
                    eprintln!("[ci] {name}: dropped output of {run}, which is not assigned to it");
                    continue;
                }
                if bytes.len() > MAX_OUTPUT_CHUNK {
                    return Err(format!("{run}: output chunk of {} bytes", bytes.len()));
                }
                let Some(wal) = wal_path(&state_dir, &run) else {
                    return Err(format!("{run} is not a run id"));
                };
                tokio::task::spawn_blocking(move || append_output(&wal, offset, &bytes))
                    .await
                    .map_err(|e| format!("the writer failed: {e}"))??;
            }
            other => {
                if scheduler
                    .send(Msg::FromRunner {
                        name: name.clone(),
                        message: other,
                    })
                    .is_err()
                {
                    return Err("the scheduler is gone".to_owned());
                }
            }
        }
    }
}

/// The path of the copy of a run's output, if `run` names a job.
fn wal_path(state_dir: &Path, run: &str) -> Option<std::path::PathBuf> {
    let (repo, number, job) = parse_run_id(run)?;
    (valid_name(&repo) && valid_name(&job))
        .then(|| transcript_dir(state_dir, &repo, number, &job).join("wal"))
}

/// Appends `bytes`, which start at byte `offset` of the run's output, to its
/// stored copy and returns the copy's new length.
///
/// A runner that resends from an earlier offset after a reconnect overlaps
/// what is held: only the part beyond it is written. One that starts past
/// the end has lost track of what was acknowledged, and the copy would no
/// longer be the runner's file: that is an error.
pub fn append_output(path: &Path, offset: u64, bytes: &[u8]) -> Result<u64, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let held = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("cannot stat {}: {e}", path.display())),
    };
    if offset > held {
        return Err(format!(
            "output at byte {offset} but {held} held for {}",
            path.display()
        ));
    }
    let skip = usize::try_from(held - offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let fresh = &bytes[skip..];
    if fresh.is_empty() {
        return Ok(held);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.write_all(fresh)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(held + fresh.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeughaus-enlist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn overlapping_output_appends_only_what_is_new() {
        let dir = scratch("append");
        let wal = dir.join("1.runs/job/wal");
        assert_eq!(append_output(&wal, 0, b"abcdefgh"), Ok(8));
        // Resent from byte 5: "fgh" is held, "ij" is new.
        assert_eq!(append_output(&wal, 5, b"fghij"), Ok(10));
        assert_eq!(append_output(&wal, 10, b"klm"), Ok(13));
        // A chunk wholly behind the end changes nothing.
        assert_eq!(append_output(&wal, 2, b"cde"), Ok(13));
        assert_eq!(std::fs::read(&wal).expect("read"), b"abcdefghijklm");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_past_the_end_is_an_error_and_writes_nothing() {
        let dir = scratch("gap");
        let wal = dir.join("wal");
        assert_eq!(append_output(&wal, 0, b"abc"), Ok(3));
        assert!(append_output(&wal, 4, b"x").is_err());
        assert!(append_output(&dir.join("other"), 1, b"x").is_err());
        assert_eq!(std::fs::read(&wal).expect("read"), b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_id_is_checked_before_it_is_a_path() {
        let state = Path::new("/state");
        assert!(wal_path(state, "repo.3.job").is_some());
        assert!(wal_path(state, "../x.3.job").is_none());
        assert!(wal_path(state, "repo.3.Job/..").is_none());
        assert!(wal_path(state, "repo.x.job").is_none());
    }
}
