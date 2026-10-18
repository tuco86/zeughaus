//! Jobs on this runner: the [`ProcessHost`] the `job` plugin executes through,
//! and the `/hold` service that stops it starting new ones.
//!
//! A run is a terminal this process owns. It has no pane until an editor
//! attaches one while the run is going, its PTY bytes are teed to
//! `<state-dir>/runs/<id>/wal` as they are parsed, each chunk with the time it
//! was read, and its exit record is written beside them. The terminal ends
//! with the program, whatever its exit code: what the run said is read from
//! its WAL.
//!
//! Run ids come from the state directory rather than from a counter that
//! starts at zero, so a restarted runner cannot hand out an id whose directory
//! already holds someone else's output.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use weida::{Replier, TransferMeta};
use zeughaus_job::{JobSpec, ProcessHost, RunExit, RunHandle, record_run_end};
use zeughaus_link::{HoldReply, HoldRequest, MAX_HOLD_BYTES};
use zeughaus_mux::{ExitState, TerminalId};
use zeughaus_terminal::Profile;
use zeughaus_terminal::wal::{Wal, WalWriter};

use crate::mux::{MuxService, OwnedPlacement, SavedRun};

/// How often a waiting run looks at its terminal. A job is measured in
/// seconds at best, so this is latency nobody can perceive and a thread that
/// costs nothing while it sleeps.
const POLL: Duration = Duration::from_millis(50);

/// Scrollback a run's terminal keeps: a failed build is read from its end,
/// and the last ten thousand rows is where the error is.
pub(crate) const RUN_SCROLLBACK_ROWS: usize = 10_000;

/// Successful runs kept on disk beyond which older ones are deleted when a
/// new run starts. Failed runs, and runs that never wrote an exit record
/// (still live, or cut short with the runner), are never pruned: those are
/// the ones someone comes back to.
pub const DEFAULT_KEEP_RUNS: usize = 50;

/// The runner's side of the job plugin's host trait.
pub struct JobHost {
    mux: MuxService,
    state_dir: PathBuf,
    /// The next run id. Seeded past every run directory that already exists.
    next_run: AtomicU64,
    keep_runs: usize,
    /// Held: start nothing new, let the live runs finish.
    held: AtomicBool,
    /// A maintenance task runs: counts as held, so no CI job or graph job
    /// starts while a toolchain may be replaced under it.
    maintaining: AtomicBool,
    /// Runs started here that have not been waited to their end.
    ///
    /// Shared with each run's handle rather than kept in the struct alone: the
    /// handle is what observes the end of a run, and stopping waits on this
    /// count.
    live: Arc<AtomicU32>,
}

impl JobHost {
    pub fn new(mux: MuxService, state_dir: PathBuf, keep_runs: usize) -> JobHost {
        let next = highest_run(&state_dir.join("runs")) + 1;
        JobHost {
            mux,
            state_dir,
            next_run: AtomicU64::new(next),
            keep_runs,
            held: AtomicBool::new(false),
            maintaining: AtomicBool::new(false),
            live: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn hold(&self, held: bool) {
        self.held.store(held, Ordering::SeqCst);
    }

    pub fn set_maintaining(&self, on: bool) {
        self.maintaining.store(on, Ordering::SeqCst);
    }

    pub fn live_runs(&self) -> u32 {
        self.live.load(Ordering::SeqCst)
    }

    /// Takes over the runs a previous runner started and did not see end:
    /// their terminals came back with the mux, and when they end, their
    /// exit record and artifacts are written here. The node that started
    /// them is gone with that runner, so its `ok`/`failed` pins do not fire
    /// for them. They count as live, so a draining stop waits for them too.
    pub fn adopt_runs(&self) {
        for (terminal, run) in self.mux.restored_runs() {
            // Recorded already: the run is over, and its terminal shows
            // nothing its WAL does not hold.
            if run.run_dir.join("exit").exists() {
                if let Err(e) = self.mux.close_terminal(terminal) {
                    eprintln!("[runner] cannot close an ended run's terminal: {e}");
                }
                continue;
            }
            self.live.fetch_add(1, Ordering::SeqCst);
            let handle = Box::new(Run {
                mux: self.mux.clone(),
                terminal,
                live: Arc::clone(&self.live),
            });
            eprintln!("[runner] adopted run {}", run.run_dir.display());
            std::thread::spawn(move || {
                let exit = handle.wait();
                if let Err(e) = record_run_end(
                    &run.run_dir,
                    exit,
                    run.started,
                    run.cwd.as_deref(),
                    &run.artifacts,
                ) {
                    eprintln!("[runner] adopted run {}: {e}", run.run_dir.display());
                }
            });
        }
    }

    /// Ends the run in `run_dir` by closing its terminal, which kills the
    /// child; the run's wait then reports it as killed.
    pub fn kill_run(&self, run_dir: &Path) -> Result<(), String> {
        let terminal = self
            .mux
            .terminal_of_run(run_dir)
            .ok_or_else(|| format!("no live terminal for {}", run_dir.display()))?;
        self.mux.close_terminal(terminal)
    }

    /// Whether a terminal of this process holds the run in `run_dir`: one
    /// it started, or one reattached from a shim. A previous process's run
    /// without either went with that process.
    pub fn has_run(&self, run_dir: &Path) -> bool {
        self.mux.terminal_of_run(run_dir).is_some()
    }

    /// Every numeric directory under `<state-dir>/runs`.
    pub fn run_dirs(&self) -> Vec<PathBuf> {
        let runs = self.state_dir.join("runs");
        run_ids(&runs)
            .into_iter()
            .map(|id| runs.join(id.to_string()))
            .collect()
    }
}

/// The numeric run directories under `runs`, newest id first. A name that is
/// not a number is not a run of this process and is ignored; an unreadable
/// directory means no runs are known, which is the same situation as a first
/// start.
fn run_ids(runs: &Path) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

fn highest_run(runs: &Path) -> u64 {
    run_ids(runs).first().copied().unwrap_or(0)
}

/// Whether the run in `dir` recorded a clean exit. Anything else -- a
/// failure, no record yet, an unreadable one -- is kept.
fn exited_clean(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("exit")).is_ok_and(|record| record.starts_with("code=0\n"))
}

/// Deletes successful runs beyond the `keep` newest. Failures to delete are
/// logged and otherwise ignored: retention is housekeeping, and a run that
/// cannot be removed today is tried again on the next run.
fn prune_runs(runs: &Path, keep: usize) {
    let mut kept = 0usize;
    for id in run_ids(runs) {
        let dir = runs.join(id.to_string());
        if !exited_clean(&dir) {
            continue;
        }
        if kept < keep {
            kept += 1;
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("[runner] cannot prune run {}: {e}", dir.display());
        }
    }
}

impl ProcessHost for JobHost {
    fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst) || self.maintaining.load(Ordering::SeqCst)
    }

    fn new_run_dir(&self) -> Result<PathBuf, String> {
        let id = self.next_run.fetch_add(1, Ordering::SeqCst);
        let runs = self.state_dir.join("runs");
        let dir = runs.join(id.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        // Housekeeping rides on the start of a run rather than on a timer:
        // the directory only grows when a run is added.
        prune_runs(&runs, self.keep_runs);
        Ok(dir)
    }

    fn spawn(&self, spec: JobSpec) -> Result<Box<dyn RunHandle>, String> {
        // Created before the child, because a run whose output cannot be
        // written is a run nobody can read afterwards -- and that is worth
        // refusing rather than discovering when it fails. The terminal
        // appends to it from here on, in whichever process holds the PTY.
        let wal_path = spec.run_dir.join("wal");
        WalWriter::open(&wal_path, None)
            .map_err(|e| format!("cannot create {}: {e}", wal_path.display()))?;
        let run = SavedRun {
            run_dir: spec.run_dir.clone(),
            started: spec.started,
            cwd: spec.cwd.clone(),
            artifacts: spec.artifacts.clone(),
        };
        // A run nobody in an editor started would otherwise sit unseen among
        // the detached terminals; one an editor pressed is attached from
        // there by whoever wants to look.
        let placement = if spec.external {
            OwnedPlacement::Triggered
        } else {
            OwnedPlacement::Detached
        };

        let mut env = spec.env;
        env.push((
            "ZEUGHAUS_RUN_DIR".to_string(),
            spec.run_dir.to_string_lossy().into_owned(),
        ));
        let profile = Profile {
            label: spec.label,
            program: Some(PathBuf::from(spec.program)),
            args: spec.args,
            cwd: spec.cwd,
            env,
            scrollback_rows: RUN_SCROLLBACK_ROWS,
        };
        let terminal = self
            .mux
            .spawn_owned(profile, Wal::At(wal_path), Some(run), placement)?;
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
                // The run is over and its log is on disk, whatever the
                // outcome: the terminal would otherwise stay for the life of
                // the runner, in the locked group or among the detached ones,
                // showing what the WAL holds.
                if let Err(e) = mux.close_terminal(terminal) {
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
    use super::*;

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use std::collections::HashMap;
        use std::io;

        use zeughaus_core::{DomainPlugin, InputSet, NodeContext, NodeId, Press, Value};
        use zeughaus_job::JobPlugin;
        use zeughaus_terminal::wal;

        /// A run's output bytes: its WAL.
        fn run_output(run_dir: &Path) -> io::Result<Vec<u8>> {
            wal::read_bytes(&run_dir.join("wal"))
        }

        fn fresh_state_dir(name: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("zeughaus-jobs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        /// Configure a `job.run` node for `sh -c <script>`, press it, and run
        /// its deferred work to the end as the host loop would.
        fn run_script(plugin: &JobPlugin, script: &str) -> HashMap<String, Value> {
            let mut node = plugin.create_node("job.run").expect("job.run exists");
            // Single quotes group the script into one argument; the scripts here
            // contain none themselves.
            node.set_parameter("command", Value::new(format!("/bin/sh -c '{script}'")))
                .unwrap();
            node.set_parameter(
                "fire",
                Value::new(Press {
                    payload: "payload-text".to_string(),
                    external: false,
                }),
            )
            .unwrap();
            let mut ctx = NodeContext::new(NodeId(1));
            node.execute(&InputSet::new(), &mut ctx).unwrap();
            ctx.take_deferred()
                .expect("a run is deferred, not answered inline")
                .run()
                .expect("the run completes")
        }

        /// The run's output once it contains `needle`, or as it is after five
        /// seconds. A run is over when its child exited, and what it printed just
        /// before can still be on its way into the WAL.
        fn output_with(run_dir: &Path, needle: &str) -> String {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let log =
                    String::from_utf8_lossy(&run_output(run_dir).unwrap_or_default()).into_owned();
                if log.contains(needle) || std::time::Instant::now() >= deadline {
                    return log;
                }
                std::thread::sleep(POLL);
            }
        }

        #[test]
        fn a_failed_run_records_its_code_and_closes_its_terminal() {
            let state_dir = fresh_state_dir("e2e");
            let mux = MuxService::new(
                crate::mux::incarnation(),
                Vec::new(),
                zeughaus_terminal::TerminalHost::Local,
            );
            let host = Arc::new(JobHost::new(
                mux.clone(),
                state_dir.clone(),
                DEFAULT_KEEP_RUNS,
            ));
            let plugin = JobPlugin::new(Arc::clone(&host) as Arc<dyn ProcessHost>);

            // First run: fails with 3. The run is over with its program, so its
            // terminal, the first the mux handed out, is gone, and the exit record
            // and the WAL say what happened.
            let outputs = run_script(
                &plugin,
                "echo marker-one got=$ZEUGHAUS_PAYLOAD in=$ZEUGHAUS_RUN_DIR; exit 3",
            );
            assert_eq!(outputs["failed"].downcast_ref::<i64>(), Some(&3));
            assert!(!outputs.contains_key("ok"));
            let run_dir = PathBuf::from(outputs["dir"].downcast_ref::<String>().unwrap());
            assert_eq!(run_dir, state_dir.join("runs").join("1"));
            let log = output_with(&run_dir, "marker-one got=payload-text in=");
            assert!(
                log.contains("marker-one got=payload-text in="),
                "log was: {log:?}"
            );
            assert!(
                log.contains(&format!("in={}", run_dir.display())),
                "log was: {log:?}"
            );
            let exit = std::fs::read_to_string(run_dir.join("exit")).unwrap();
            assert!(
                exit.starts_with("code=3\nkilled=false\n"),
                "exit was: {exit:?}"
            );
            assert!(!run_dir.join("code").exists());
            assert!(
                mux.session(TerminalId(1)).is_none(),
                "the failed run's terminal is closed"
            );
            assert_eq!(host.live_runs(), 0);

            // Second run: succeeds, and ends the same way.
            let outputs = run_script(&plugin, "echo marker-two");
            assert_eq!(outputs["ok"].downcast_ref::<bool>(), Some(&true));
            let run_dir = PathBuf::from(outputs["dir"].downcast_ref::<String>().unwrap());
            assert_eq!(run_dir, state_dir.join("runs").join("2"));
            assert!(output_with(&run_dir, "marker-two").contains("marker-two"));
            assert!(!run_dir.join("code").exists());
            assert!(mux.session(TerminalId(2)).is_none());

            // A restarted host continues the numbering past what is on disk.
            let restarted = JobHost::new(mux, state_dir.clone(), DEFAULT_KEEP_RUNS);
            assert_eq!(
                restarted.new_run_dir().unwrap(),
                state_dir.join("runs").join("3")
            );

            let _ = std::fs::remove_dir_all(&state_dir);
        }

        #[test]
        fn a_held_host_starts_nothing() {
            let state_dir = fresh_state_dir("held");
            let mux = MuxService::new(
                crate::mux::incarnation(),
                Vec::new(),
                zeughaus_terminal::TerminalHost::Local,
            );
            let host = Arc::new(JobHost::new(mux, state_dir.clone(), DEFAULT_KEEP_RUNS));
            host.hold(true);
            let plugin = JobPlugin::new(Arc::clone(&host) as Arc<dyn ProcessHost>);
            let mut node = plugin.create_node("job.run").unwrap();
            node.set_parameter("command", Value::new("/bin/true".to_string()))
                .unwrap();
            node.set_parameter("fire", Value::new(Press::default()))
                .unwrap();
            let mut ctx = NodeContext::new(NodeId(1));
            let err = node.execute(&InputSet::new(), &mut ctx).unwrap_err();
            assert!(err.to_string().contains("held"), "was: {err}");
            assert!(ctx.take_deferred().is_none());
            assert!(!state_dir.join("runs").exists());
        }

        #[test]
        fn only_successful_runs_beyond_the_newest_are_pruned() {
            let state_dir = fresh_state_dir("prune");
            let runs = state_dir.join("runs");
            let record = |id: u64, exit: Option<&str>| {
                let dir = runs.join(id.to_string());
                std::fs::create_dir_all(&dir).unwrap();
                if let Some(exit) = exit {
                    std::fs::write(dir.join("exit"), exit).unwrap();
                }
            };
            // Oldest to newest: green, red, green, no record (cut short), green,
            // green.
            record(1, Some("code=0\nkilled=false\n"));
            record(2, Some("code=3\nkilled=false\n"));
            record(3, Some("code=0\nkilled=false\n"));
            record(4, None);
            record(5, Some("code=0\nkilled=false\n"));
            record(6, Some("code=0\nkilled=false\n"));

            let mux = MuxService::new(
                crate::mux::incarnation(),
                Vec::new(),
                zeughaus_terminal::TerminalHost::Local,
            );
            let host = JobHost::new(mux, state_dir.clone(), 2);
            let fresh = host.new_run_dir().unwrap();
            assert_eq!(fresh, runs.join("7"));

            let left = run_ids(&runs);
            // The two newest green runs stay, the older green ones go, and the
            // failed one, the recordless one and the new one are untouched.
            assert_eq!(left, vec![7, 6, 5, 4, 2]);

            let _ = std::fs::remove_dir_all(&state_dir);
        }
    }
}
