//! The connection to the CI: dialed, kept and redialed by this runner.
//!
//! One exchange on `/enlist` lasts as long as the connection: `Hello` out,
//! `Welcome` back, then the runner's reports one way and the CI's commands the
//! other. Frames are read by a task of their own: a frame is a length and a
//! body, and a `select!` that cancelled between the two would leave the
//! stream in the middle of a message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use weida::{GiveUp, PeerEvent, Requester, TransferMeta};
use zeughaus_link::ENLIST_PATH;
use zeughaus_link::enlist::{ENLIST_MAJOR, FromRunner, ToRunner, read_frame, write_frame};

use super::{Executor, lock};

/// The first redial delay, doubled per failure up to [`MAX_DELAY`].
const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);

/// A connection that lasted this long was a working one: the next loss starts
/// the delays over.
const STABLE_UPTIME: Duration = Duration::from_secs(60);

/// Frames waiting for the connection to write them. Bounded, so a slow link
/// slows the threads that stream logs instead of growing this process.
const QUEUE: usize = 64;

/// The delay before attempt `attempt` (from 1): 1 s, doubling, at most 30 s.
fn delay(attempt: u32) -> Duration {
    FIRST_DELAY
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(8))
        .min(MAX_DELAY)
}

/// The attempt number after an exchange that lasted `lasted`.
fn next_attempt(attempt: u32, lasted: Duration) -> u32 {
    if lasted >= STABLE_UPTIME {
        1
    } else {
        attempt.saturating_add(1)
    }
}

/// How an exchange ended.
struct Ended {
    cause: String,
    /// The CI turned this runner away; said as `refused`, not `lost`.
    refused: bool,
    /// Weida stopped redialling the address: it has to be dialled again.
    gave_up: bool,
}

impl Ended {
    fn lost(cause: impl Into<String>) -> Ended {
        Ended {
            cause: cause.into(),
            refused: false,
            gave_up: false,
        }
    }
}

/// A spawned half of an exchange, stopped when its owner goes away.
struct Abort(tokio::task::JoinHandle<Ended>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Keeps the connection to the CI for as long as the process lives.
pub async fn keep(exec: Arc<Executor>) {
    let url = match exec.config.ci_url(ENLIST_PATH) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("[enlist] lost: {e}");
            return;
        }
    };
    let requester = exec.runtime.requester(exec.tls.clone());
    let mut events = requester.events();
    let mut dialled = false;
    let mut attempt = 0u32;
    loop {
        if attempt > 0 {
            tokio::time::sleep(delay(attempt)).await;
        }
        let started = Instant::now();
        if !dialled {
            // Weida redials a lost address by itself, but only once a first
            // dial worked: a CI that is not up yet is asked again from here.
            match requester.connect(&url).await {
                Ok(()) => dialled = true,
                Err(e) => {
                    eprintln!("[enlist] lost: connect {url}: {e}");
                    attempt = next_attempt(attempt, started.elapsed());
                    continue;
                }
            }
        }
        let ended = exchange(&exec, &requester, &mut events).await;
        if ended.refused {
            eprintln!("[enlist] refused: {}", ended.cause);
        } else {
            eprintln!("[enlist] lost: {}", ended.cause);
        }
        if ended.gave_up {
            requester.disconnect(&url);
            dialled = false;
        }
        attempt = next_attempt(attempt, started.elapsed());
    }
}

/// One exchange, from the open to whatever ends it.
async fn exchange(
    exec: &Arc<Executor>,
    requester: &Requester,
    events: &mut weida::PeerEvents,
) -> Ended {
    let (mut request, reply) = match requester.open(TransferMeta::default()).await {
        Ok(halves) => halves,
        Err(e) => return Ended::lost(format!("open: {e}")),
    };
    let hello = FromRunner::Hello {
        major: ENLIST_MAJOR,
        name: exec.config.name.clone(),
        offer: exec.offer(),
        runs: exec.reports(),
        artifacts: exec.artifact_sets(),
    };
    if let Err(e) = write_frame(&mut request, &hello).await {
        return Ended::lost(format!("hello: {e}"));
    }
    let mut body = match reply.recv().await {
        Ok(body) => body,
        Err(e) => return Ended::lost(format!("welcome: {e}")),
    };
    let offsets = match read_frame::<_, ToRunner>(&mut body).await {
        Ok(Some(ToRunner::Welcome { major, offsets })) if major == ENLIST_MAJOR => offsets,
        Ok(Some(ToRunner::Welcome { major, .. })) => {
            return Ended::lost(format!(
                "the CI speaks enlist protocol {major}, this runner {ENLIST_MAJOR}"
            ));
        }
        Ok(Some(ToRunner::Refused { message })) => {
            return Ended {
                cause: message,
                refused: true,
                gave_up: false,
            };
        }
        Ok(Some(_)) => return Ended::lost("the CI answered hello with something else"),
        Ok(None) => return Ended::lost("the CI ended the exchange before it welcomed"),
        Err(e) => return Ended::lost(format!("welcome: {e}")),
    };

    let (tx, mut queue) = mpsc::channel::<FromRunner>(QUEUE);
    let generation = {
        let mut link = lock(&exec.link);
        link.generation += 1;
        link.tx = Some(tx);
        link.offsets = offsets.into_iter().collect();
        link.generation
    };
    eprintln!(
        "[enlist] connected to {} as {}",
        exec.config.ci, exec.config.name
    );
    let mut reader = Abort(tokio::spawn(read_commands(body, Arc::clone(exec))));
    let ended = loop {
        tokio::select! {
            message = queue.recv() => {
                let Some(message) = message else {
                    break Ended::lost("the send queue closed");
                };
                if let Err(e) = write_frame(&mut request, &message).await {
                    break Ended::lost(format!("write: {e}"));
                }
            }
            ended = &mut reader.0 => {
                break ended.unwrap_or_else(|e| Ended::lost(format!("reader: {e}")));
            }
            event = events.recv() => match event {
                None => break Ended::lost("the endpoint is gone"),
                Some(PeerEvent::Lost { cause, .. }) => break Ended::lost(cause.to_string()),
                Some(PeerEvent::GaveUp { why, .. }) => {
                    break Ended {
                        cause: explain(&why),
                        refused: false,
                        gave_up: true,
                    };
                }
                Some(_) => {}
            },
        }
    };
    // A connection that is not this one any more must not be detached: the
    // next exchange may already have attached.
    let mut link = lock(&exec.link);
    if link.generation == generation {
        link.tx = None;
    }
    ended
}

/// Reads the CI's commands until the exchange ends.
async fn read_commands(mut body: weida::IncomingTransfer, exec: Arc<Executor>) -> Ended {
    loop {
        match read_frame::<_, ToRunner>(&mut body).await {
            Ok(Some(command)) => dispatch(&exec, command),
            Ok(None) => return Ended::lost("the CI ended the exchange"),
            Err(e) => return Ended::lost(e),
        }
    }
}

/// Acts on one command. Quick: anything that takes time runs on a thread.
fn dispatch(exec: &Arc<Executor>, command: ToRunner) {
    match command {
        ToRunner::Assign { assignment } => exec.assign(*assignment),
        ToRunner::Kill { run } => exec.kill(&run),
        ToRunner::Upload { run, transfer } => exec.upload(run, transfer),
        ToRunner::Prefetch {
            run,
            consumer,
            need,
        } => exec.prefetch(run, consumer, need),
        ToRunner::DropArtifacts { run } => exec.drop_artifacts(&run),
        ToRunner::DropPrefetch { consumer } => exec.drop_prefetch(&consumer),
        ToRunner::Forget { run } => exec.forget(&run),
        ToRunner::Welcome { .. } | ToRunner::Refused { .. } => {
            eprintln!("[enlist] the CI repeated its answer to hello; ignored");
        }
    }
}

/// Why weida stopped redialling, in a log line's words.
fn explain(why: &GiveUp) -> String {
    match why {
        GiveUp::PeerChanged {
            presented: Some(fp),
        } => format!("a different CI answers at this address ({fp}); is enlist.toml stale?"),
        GiveUp::PeerChanged { presented: None } => {
            "something that proves no identity answers at this address".to_owned()
        }
        GiveUp::Failed(reason) => format!("the redial cannot be used: {reason}"),
        GiveUp::Policy { attempts } => format!("given up after {attempts} redial(s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redials_double_from_a_second_to_half_a_minute() {
        let seconds: Vec<u64> = (1..=8).map(|n| delay(n).as_secs()).collect();
        assert_eq!(seconds, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_minute_of_uptime_starts_the_delays_over() {
        assert_eq!(next_attempt(5, Duration::from_secs(59)), 6);
        assert_eq!(next_attempt(5, Duration::from_secs(60)), 1);
    }
}
