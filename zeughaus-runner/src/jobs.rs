//! Jobs on this runner: the [`ProcessHost`] the `job` plugin executes through,
//! and the `/hold` service that stops it starting new ones.
//!
//! A run is a terminal this process owns. It has no pane until an editor
//! attaches one, its PTY bytes are teed to `<state-dir>/runs/<id>/log` as they
//! are parsed, and its exit record is written beside them. That is what makes
//! a failed job a terminal to attach to rather than a log to read: the screen
//! the child left behind survives the child.
//!
//! Run ids come from the state directory rather than from a counter that
//! starts at zero, so a restarted runner cannot hand out an id whose directory
//! already holds someone else's log.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use weida::{Replier, TransferMeta};
use zeughaus_job::{JobSpec, ProcessHost, RunExit, RunHandle};
use zeughaus_link::{HoldReply, HoldRequest, MAX_HOLD_BYTES};
use zeughaus_mux::{ExitState, TerminalId};
use zeughaus_terminal::Profile;

use crate::mux::MuxService;

/// How often a waiting run looks at its terminal. A job is measured in
/// seconds at best, so this is latency nobody can perceive and a thread that
/// costs nothing while it sleeps.
const POLL: Duration = Duration::from_millis(50);

/// Scrollback a run's terminal keeps: a failed build is read from its end,
/// and the last ten thousand rows is where the error is.
const RUN_SCROLLBACK_ROWS: usize = 10_000;

/// The runner's side of the job plugin's host trait.
pub struct JobHost {
    mux: MuxService,
    state_dir: PathBuf,
    /// The next run id. Seeded past every run directory that already exists.
    next_run: AtomicU64,
    /// Held: start nothing new, let the live runs finish.
    held: AtomicBool,
    /// Runs started here that have not been waited to their end.
    ///
    /// Shared with each run's handle rather than kept in the struct alone: the
    /// handle is what observes the end of a run, and stopping waits on this
    /// count.
    live: Arc<AtomicU32>,
}

impl JobHost {
    pub fn new(mux: MuxService, state_dir: PathBuf) -> JobHost {
        let next = highest_run(&state_dir.join("runs")) + 1;
        JobHost {
            mux,
            state_dir,
            next_run: AtomicU64::new(next),
            held: AtomicBool::new(false),
            live: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn hold(&self, held: bool) {
        self.held.store(held, Ordering::SeqCst);
    }

    pub fn live_runs(&self) -> u32 {
        self.live.load(Ordering::SeqCst)
    }
}

/// The largest numeric run directory under `runs`, or 0 when there is none.
///
/// A name that is not a number is not a run of this process and is ignored;
/// an unreadable directory means no runs are known, which is the same
/// situation as a first start.
fn highest_run(runs: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

impl ProcessHost for JobHost {
    fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn new_run_dir(&self) -> Result<PathBuf, String> {
        let id = self.next_run.fetch_add(1, Ordering::SeqCst);
        let dir = self.state_dir.join("runs").join(id.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(dir)
    }

    fn spawn(&self, spec: JobSpec) -> Result<Box<dyn RunHandle>, String> {
        // Opened before the child, because a run whose log cannot be written
        // is a run nobody can read afterwards -- and that is worth refusing
        // rather than discovering when it fails.
        let log_path = spec.run_dir.join("log");
        let log = File::create(&log_path)
            .map_err(|e| format!("cannot create {}: {e}", log_path.display()))?;

        let profile = Profile {
            label: spec.label,
            program: Some(PathBuf::from(spec.program)),
            args: spec.args,
            cwd: spec.cwd,
            env: spec.env,
            scrollback_rows: RUN_SCROLLBACK_ROWS,
        };
        let terminal = self.mux.spawn_owned(profile, Some(Box::new(log)))?;
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Run {
            mux: self.mux.clone(),
            terminal,
            live: Arc::clone(&self.live),
        }))
    }
}

/// Decrements the live count however the wait ended, panic included: a count
/// left standing would make a draining runner wait forever.
struct LiveGuard(Arc<AtomicU32>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A started run, waited for on the blocking pool.
struct Run {
    mux: MuxService,
    terminal: TerminalId,
    live: Arc<AtomicU32>,
}

impl RunHandle for Run {
    fn wait(self: Box<Self>) -> RunExit {
        let Run {
            mux,
            terminal,
            live,
        } = *self;
        let _guard = LiveGuard(live);
        loop {
            // Gone means someone closed the terminal -- a `CloseTerminal`
            // command, or the runner shutting the mux down. Either way the
            // child did not decide how this run ended.
            let Some(session) = mux.session(terminal) else {
                return RunExit {
                    code: None,
                    killed: true,
                };
            };
            if let Some(exit) = session.exit() {
                let run = match exit {
                    ExitState::Exited { code } => RunExit {
                        code: Some(i32::try_from(code).unwrap_or(-1)),
                        killed: false,
                    },
                    // A signal is not an exit code, and a job that was
                    // signalled did not succeed: no code, not deliberate.
                    ExitState::Signaled { .. } => RunExit {
                        code: None,
                        killed: false,
                    },
                    ExitState::Killed => RunExit {
                        code: None,
                        killed: true,
                    },
                    ExitState::SpawnFailed { .. } => RunExit {
                        code: None,
                        killed: false,
                    },
                };
                // A run that succeeded has nothing left to look at: its log
                // is on disk, and its terminal would otherwise sit among the
                // detached ones for the life of the runner. A failed one
                // keeps its screen -- that is the terminal someone attaches
                // to, and it stays until the run is deleted.
                if run.code == Some(0)
                    && let Err(e) = mux.close_terminal(terminal)
                {
                    eprintln!("[runner] cannot close a finished run's terminal: {e}");
                }
                return run;
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Answers hold requests: sets the flag and reports what the runner is doing.
///
/// The reply carries the live run count because "held" alone does not answer
/// the question that is actually being asked -- whether it is safe to stop
/// this process.
pub async fn serve_hold(replier: Replier, host: Arc<JobHost>) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[runner] stopped serving hold: {e}");
                return;
            }
        };
        let payload = match request.take_body().collect(MAX_HOLD_BYTES).await {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!("[runner] unreadable hold request: {e}");
                continue;
            }
        };
        let Some(hold) = HoldRequest::decode(&payload) else {
            eprintln!(
                "[runner] refused a malformed hold request ({} bytes)",
                payload.len()
            );
            continue;
        };
        host.hold(hold.held);
        let reply_body = HoldReply {
            held: host.held(),
            live_runs: host.live_runs(),
        };
        eprintln!(
            "[runner] {} ({} live)",
            if reply_body.held { "held" } else { "released" },
            reply_body.live_runs
        );
        let encoded = reply_body.encode();
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[runner] cannot reply to a hold request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&encoded).await {
            eprintln!("[runner] cannot write a hold reply: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[runner] cannot finish a hold reply: {e}");
        }
    }
}

/// A real child in a real PTY through the whole path: the `job.run` node,
/// this host, the mux and the terminal engine. Unix only, like the engine's
/// own tests: it drives `/bin/sh`.
#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;

    use zeughaus_core::{DomainPlugin, InputSet, NodeContext, NodeId, Value};
    use zeughaus_job::JobPlugin;

    use super::*;

    fn fresh_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zeughaus-jobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Configure a `job.run` node for `sh -c <script>`, press it, and run
    /// its deferred work to the end as the host loop would.
    fn run_script(plugin: &JobPlugin, script: &str) -> HashMap<String, Value> {
        let mut node = plugin.create_node("job.run").expect("job.run exists");
        node.set_parameter("command", Value::new("/bin/sh".to_string()))
            .unwrap();
        node.set_parameter("args", Value::new(format!("-c\n{script}")))
            .unwrap();
        node.set_parameter("fire", Value::new(String::new()))
            .unwrap();
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        ctx.take_deferred()
            .expect("a run is deferred, not answered inline")
            .run()
            .expect("the run completes")
    }

    #[test]
    fn a_run_is_logged_and_only_a_failed_one_keeps_its_terminal() {
        let state_dir = fresh_state_dir("e2e");
        let mux = MuxService::new(crate::mux::incarnation(), Vec::new());
        let host = Arc::new(JobHost::new(mux.clone(), state_dir.clone()));
        let plugin = JobPlugin::new(Arc::clone(&host) as Arc<dyn ProcessHost>);

        // First run: fails with 3. Its terminal is the first the mux hands
        // out and must still be there, holding the screen.
        let outputs = run_script(&plugin, "echo marker-one; exit 3");
        assert_eq!(outputs["failed"].downcast_ref::<i64>(), Some(&3));
        assert!(!outputs.contains_key("ok"));
        let run_dir = PathBuf::from(outputs["run"].downcast_ref::<String>().unwrap());
        assert_eq!(run_dir, state_dir.join("runs").join("1"));
        let log = std::fs::read_to_string(run_dir.join("log")).unwrap();
        assert!(log.contains("marker-one"), "log was: {log:?}");
        let exit = std::fs::read_to_string(run_dir.join("exit")).unwrap();
        assert!(
            exit.starts_with("code=3\nkilled=false\n"),
            "exit was: {exit:?}"
        );
        assert!(mux.session(TerminalId(1)).is_some());
        assert_eq!(host.live_runs(), 0);

        // Second run: succeeds. Its terminal is closed, the log stays.
        let outputs = run_script(&plugin, "echo marker-two");
        assert_eq!(outputs["ok"].downcast_ref::<bool>(), Some(&true));
        let run_dir = PathBuf::from(outputs["run"].downcast_ref::<String>().unwrap());
        assert_eq!(run_dir, state_dir.join("runs").join("2"));
        assert!(
            std::fs::read_to_string(run_dir.join("log"))
                .unwrap()
                .contains("marker-two")
        );
        assert!(mux.session(TerminalId(2)).is_none());
        assert!(mux.session(TerminalId(1)).is_some());

        // A restarted host continues the numbering past what is on disk.
        let restarted = JobHost::new(mux, state_dir.clone());
        assert_eq!(
            restarted.new_run_dir().unwrap(),
            state_dir.join("runs").join("3")
        );

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn a_held_host_starts_nothing() {
        let state_dir = fresh_state_dir("held");
        let mux = MuxService::new(crate::mux::incarnation(), Vec::new());
        let host = Arc::new(JobHost::new(mux, state_dir.clone()));
        host.hold(true);
        let plugin = JobPlugin::new(Arc::clone(&host) as Arc<dyn ProcessHost>);
        let mut node = plugin.create_node("job.run").unwrap();
        node.set_parameter("command", Value::new("/bin/true".to_string()))
            .unwrap();
        node.set_parameter("fire", Value::new(String::new()))
            .unwrap();
        let mut ctx = NodeContext::new(NodeId(1));
        let err = node.execute(&InputSet::new(), &mut ctx).unwrap_err();
        assert!(err.to_string().contains("held"), "was: {err}");
        assert!(ctx.take_deferred().is_none());
        assert!(!state_dir.join("runs").exists());
    }
}
