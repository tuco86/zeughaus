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
use zeughaus_job::{JobSpec, ProcessHost, RunExit, RunHandle, record_run_end};
use zeughaus_link::{HoldReply, HoldRequest, MAX_HOLD_BYTES};
use zeughaus_mux::{ExitState, TerminalId};
use zeughaus_terminal::Profile;

use crate::mux::{MuxService, OwnedPlacement, SavedRun};

/// How often a waiting run looks at its terminal. A job is measured in
/// seconds at best, so this is latency nobody can perceive and a thread that
/// costs nothing while it sleeps.
const POLL: Duration = Duration::from_millis(50);

/// Scrollback a run's terminal keeps: a failed build is read from its end,
/// and the last ten thousand rows is where the error is.
const RUN_SCROLLBACK_ROWS: usize = 10_000;

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
            live: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn hold(&self, held: bool) {
        self.held.store(held, Ordering::SeqCst);
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
            // Recorded already: it ended under the previous runner. A failure
            // that a shell followed still has that shell's terminal, which
            // closes when the shell ends, as it would have there.
            if run.run_dir.join("exit").exists() {
                if run.run_dir.join("code").exists() {
                    close_after_shell(self.mux.clone(), terminal);
                }
                continue;
            }
            self.live.fetch_add(1, Ordering::SeqCst);
            let handle = Box::new(Run {
                mux: self.mux.clone(),
                terminal,
                code_file: run.run_dir.join("code"),
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
        self.held.load(Ordering::SeqCst)
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
        // Created before the child, because a run whose log cannot be
        // written is a run nobody can read afterwards -- and that is worth
        // refusing rather than discovering when it fails. The terminal
        // appends to it from here on, in whichever process holds the PTY.
        let log_path = spec.run_dir.join("log");
        File::create(&log_path)
            .map_err(|e| format!("cannot create {}: {e}", log_path.display()))?;
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

        let (program, args, mut env) = if spec.keep_on_failure && cfg!(unix) {
            // The wrapper runs the program, and on failure records the code
            // where `wait` finds it and becomes the user's shell in the same
            // directory and environment. On success it exits 0 like the
            // program did.
            let mut args = vec![
                "-c".to_string(),
                KEEP_SHELL.to_string(),
                "zeughaus-job".to_string(),
                spec.program,
            ];
            args.extend(spec.args);
            ("/bin/sh".to_string(), args, spec.env)
        } else {
            (spec.program, spec.args, spec.env)
        };
        env.push((
            "ZEUGHAUS_RUN_DIR".to_string(),
            spec.run_dir.to_string_lossy().into_owned(),
        ));
        let profile = Profile {
            label: spec.label,
            program: Some(PathBuf::from(program)),
            args,
            cwd: spec.cwd,
            env,
            scrollback_rows: RUN_SCROLLBACK_ROWS,
        };
        let terminal = self
            .mux
            .spawn_owned(profile, Some(log_path), Some(run), placement)?;
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Run {
            mux: self.mux.clone(),
            terminal,
            code_file: spec.run_dir.join("code"),
            live: Arc::clone(&self.live),
        }))
    }
}

/// The shell script a `keep_on_failure` run is started through: `$0` is a
/// name for error messages, `$@` the program and its arguments. The code
/// file is what tells `wait` that the run is over while the shell that
/// replaced it lives on; it is written before the shell so the two cannot
/// be observed in the wrong order.
const KEEP_SHELL: &str = r##""$@"; rc=$?
if [ "$rc" -eq 0 ]; then exit 0; fi
printf '%s\n' "$rc" > "$ZEUGHAUS_RUN_DIR/code"
printf '\n[zeughaus] exit %s; a shell follows\n' "$rc"
exec "${SHELL:-/bin/sh}""##;

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
    /// Written by the wrapper when the program failed and a shell took its
    /// place; the run is over even though the terminal's child is not.
    code_file: PathBuf,
    live: Arc<AtomicU32>,
}

impl RunHandle for Run {
    fn wait(self: Box<Self>) -> RunExit {
        let Run {
            mux,
            terminal,
            code_file,
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
            if let Some(code) = std::fs::read_to_string(&code_file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                close_after_shell(mux, terminal);
                return RunExit {
                    code: Some(code),
                    killed: false,
                };
            }
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
                // to.
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

/// Closes `terminal` once the shell that followed a failed run has ended.
///
/// The shell is there to look around in the run's environment; once it is
/// gone, the screen shows nothing the log under the run directory does not,
/// and a terminal left behind for every red build piles up in the
/// `Triggered` group for the life of the runner and across its restarts.
/// A terminal someone closed first, or a mux shutting down, ends the wait.
fn close_after_shell(mux: MuxService, terminal: TerminalId) {
    let spawned = std::thread::Builder::new()
        .name(format!("zh-run-close-{}", terminal.0))
        .spawn(move || {
            loop {
                let Some(session) = mux.session(terminal) else {
                    return;
                };
                if session.exit().is_some() {
                    break;
                }
                drop(session);
                std::thread::sleep(POLL);
            }
            if let Err(e) = mux.close_terminal(terminal) {
                eprintln!("[runner] cannot close a failed run's terminal: {e}");
            }
        });
    if let Err(e) = spawned {
        eprintln!("[runner] no thread to close a failed run's terminal: {e}");
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

    use zeughaus_core::{DomainPlugin, InputSet, NodeContext, NodeId, Press, Value};
    use zeughaus_job::JobPlugin;

    use super::*;

    fn fresh_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zeughaus-jobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Configure a `job.run` node for `sh -c <script>`, press it, and run
    /// its deferred work to the end as the host loop would.
    fn run_script(plugin: &JobPlugin, script: &str, keep: bool) -> HashMap<String, Value> {
        let mut node = plugin.create_node("job.run").expect("job.run exists");
        // Single quotes group the script into one argument; the scripts here
        // contain none themselves.
        node.set_parameter("command", Value::new(format!("/bin/sh -c '{script}'")))
            .unwrap();
        node.set_parameter("keep_on_failure", Value::new(keep.to_string()))
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

    #[test]
    fn a_run_is_logged_and_only_a_failed_one_keeps_its_terminal() {
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

        // First run: fails with 3. Its terminal is the first the mux hands
        // out and must still be there -- with a shell in it, since the run
        // keeps on failure: the child has not exited even though the run is
        // over and reported.
        let outputs = run_script(
            &plugin,
            "echo marker-one got=$ZEUGHAUS_PAYLOAD in=$ZEUGHAUS_RUN_DIR; exit 3",
            true,
        );
        assert_eq!(outputs["failed"].downcast_ref::<i64>(), Some(&3));
        assert!(!outputs.contains_key("ok"));
        let run_dir = PathBuf::from(outputs["dir"].downcast_ref::<String>().unwrap());
        assert_eq!(run_dir, state_dir.join("runs").join("1"));
        let log = std::fs::read_to_string(run_dir.join("log")).unwrap();
        assert!(
            log.contains("marker-one got=payload-text in="),
            "log was: {log:?}"
        );
        assert!(
            log.contains(&format!("in={}", run_dir.display())),
            "log was: {log:?}"
        );
        assert!(log.contains("a shell follows"), "log was: {log:?}");
        let exit = std::fs::read_to_string(run_dir.join("exit")).unwrap();
        assert!(
            exit.starts_with("code=3\nkilled=false\n"),
            "exit was: {exit:?}"
        );
        let kept = mux
            .session(TerminalId(1))
            .expect("the failed run's terminal");
        assert!(kept.exit().is_none(), "the shell should still be running");
        assert_eq!(host.live_runs(), 0);

        // Second run: succeeds. Its terminal is closed, the log stays, and no
        // code file was written because the wrapper exited with the program.
        let outputs = run_script(&plugin, "echo marker-two", true);
        assert_eq!(outputs["ok"].downcast_ref::<bool>(), Some(&true));
        let run_dir = PathBuf::from(outputs["dir"].downcast_ref::<String>().unwrap());
        assert_eq!(run_dir, state_dir.join("runs").join("2"));
        assert!(
            std::fs::read_to_string(run_dir.join("log"))
                .unwrap()
                .contains("marker-two")
        );
        assert!(!run_dir.join("code").exists());
        assert!(mux.session(TerminalId(2)).is_none());
        assert!(mux.session(TerminalId(1)).is_some());

        // Third run: fails without keeping. The terminal stays with its
        // final screen, but nothing runs in it any more.
        let outputs = run_script(&plugin, "exit 7", false);
        assert_eq!(outputs["failed"].downcast_ref::<i64>(), Some(&7));
        let dead = mux.session(TerminalId(3)).expect("the terminal is kept");
        assert!(dead.exit().is_some());

        // Ending the shell that followed the first failure closes its
        // terminal: the log holds everything it showed.
        kept.apply(&zeughaus_mux::TerminalCommand::Text {
            serial: 1,
            text: "exit\r".to_string(),
        })
        .unwrap();
        drop(kept);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while mux.session(TerminalId(1)).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the failed run's terminal outlived its shell"
            );
            std::thread::sleep(POLL);
        }
        assert!(mux.session(TerminalId(3)).is_some());

        // A restarted host continues the numbering past what is on disk.
        let restarted = JobHost::new(mux, state_dir.clone(), DEFAULT_KEEP_RUNS);
        assert_eq!(
            restarted.new_run_dir().unwrap(),
            state_dir.join("runs").join("4")
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
