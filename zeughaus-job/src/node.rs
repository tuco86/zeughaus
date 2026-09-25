use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use zeughaus_core::*;

use crate::{JobSpec, ProcessHost, RunExit, RunHandle};

/// One process with a beginning and an end.
///
/// Fires on an event -- a value delivered on `run`, or a press from an editor
/// -- and never on its own: a job with a clock would be a cron entry, and
/// re-running because something upstream recomputed would start a build per
/// keystroke.
///
/// The wait is deferred ([`AsyncWork`]), so the pass that started the run ends
/// immediately and the outputs arrive when the child exits. Exactly one run at
/// a time: the live flag is shared with the deferred work, which is the only
/// thing that clears it.
pub struct JobNode {
    /// `None` in a process that does not execute. See the crate docs.
    host: Option<Arc<dyn ProcessHost>>,
    /// The program and its arguments, already split.
    command: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
    artifacts: Vec<String>,
    keep_on_failure: bool,
    /// A press taken through the parameter channel, spent by the next execute.
    armed: bool,
    /// The text the press carried, if any; spent with it.
    payload: Option<String>,
    /// Whether the press came from outside an editor; spent with it, so a
    /// run the `run` pin starts is never external.
    external: bool,
    /// Whether a run started here is still going. Shared with its deferred
    /// work, which clears it however the run ended.
    live: Arc<AtomicBool>,
    pins: Vec<PinDefinition>,
}

impl JobNode {
    pub fn new(host: Option<Arc<dyn ProcessHost>>) -> Self {
        Self {
            host,
            command: Vec::new(),
            env: Vec::new(),
            cwd: String::new(),
            artifacts: Vec::new(),
            keep_on_failure: true,
            armed: false,
            payload: None,
            external: false,
            live: Arc::new(AtomicBool::new(false)),
            pins: vec![
                PinDefinition::input("run", Ty::Any, PinKind::Trigger),
                // State, not an event: where to run is read when the run
                // starts, whatever produced it and whenever it did.
                PinDefinition::input("cwd", Ty::Str, PinKind::Sample),
                // `ok` and `failed` are two pins rather than one status value
                // because what hangs off them is trigger wiring: the success
                // path fires on `ok` and the recovery path on `failed`, and a
                // single pin would deliver an event to both and leave each
                // downstream node to decide whether this one was for it.
                PinDefinition::output("ok", Ty::Bool),
                PinDefinition::output("failed", Ty::Int),
                // Not `run`: the editor addresses pins by name, and the
                // trigger input already has that one.
                PinDefinition::output("dir", Ty::Str),
            ],
        }
    }
}

impl ExecutableNode for JobNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // The press is spent whether or not the run starts: a refusal is
        // reported once, not re-tried on the next unrelated pass.
        let pressed = std::mem::take(&mut self.armed);
        let payload = std::mem::take(&mut self.payload);
        let external = std::mem::take(&mut self.external);
        if !pressed && !inputs.changed("run") {
            return Ok(());
        }
        // What fired the run reaches the program as `ZEUGHAUS_PAYLOAD`: the
        // text an external trigger sent, or the string that arrived on
        // `run`. A value of any other type is the event only.
        let payload = payload.or_else(|| inputs.get::<String>("run"));

        let Some(host) = &self.host else {
            return Err(ZeughausError::ExecutionFailed(
                "job: this process does not execute".to_string(),
            ));
        };
        if host.held() {
            return Err(ZeughausError::ExecutionFailed(
                "job: the runner is held and starts no new runs".to_string(),
            ));
        }
        let Some(program) = self.command.first().map(String::as_str) else {
            return Err(ZeughausError::ExecutionFailed(
                "job: no command set".to_string(),
            ));
        };
        // One run at a time per node. A queue depth is a later setting; until
        // then a trigger arriving mid-run is an error the editor sees, not a
        // second process.
        if self.live.load(Ordering::Acquire) {
            return Err(ZeughausError::ExecutionFailed(
                "job busy: the previous run has not finished".to_string(),
            ));
        }

        let run_dir = host.new_run_dir().map_err(ZeughausError::ExecutionFailed)?;
        // A wired `cwd` wins over the setting: that is how a checkout
        // upstream hands its directory to the job that builds in it.
        let cwd_text = inputs
            .get::<String>("cwd")
            .unwrap_or_else(|| self.cwd.clone());
        let cwd = match cwd_text.trim() {
            "" => None,
            dir => Some(PathBuf::from(dir)),
        };
        let mut env = self.env.clone();
        if let Some(payload) = payload {
            env.push(("ZEUGHAUS_PAYLOAD".to_string(), payload));
        }
        let label = Path::new(program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.to_string());

        let started = unix_secs();
        let handle = host
            .spawn(JobSpec {
                label,
                program: program.to_string(),
                args: self.command[1..].to_vec(),
                env,
                cwd: cwd.clone(),
                run_dir: run_dir.clone(),
                keep_on_failure: self.keep_on_failure,
                started,
                artifacts: self.artifacts.clone(),
                external,
            })
            .map_err(ZeughausError::ExecutionFailed)?;

        self.live.store(true, Ordering::Release);
        ctx.defer(Box::new(RunWork {
            handle,
            live: Arc::clone(&self.live),
            run_dir,
            cwd,
            artifacts: self.artifacts.clone(),
            started,
        }));
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("command", "").placeholder("program arg \"quoted arg\""),
            SettingDef::new("env", "").placeholder("KEY=VALUE KEY2=\"a b\""),
            SettingDef::new("cwd", "").placeholder("(the runner's directory)"),
            SettingDef::new("artifacts", "").placeholder("globs relative to cwd"),
            SettingDef::new("keep_on_failure", "true"),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        // The press itself is the signal; text that came with it is kept for
        // the run it starts, and an empty press carries none.
        if name == "fire" {
            let press = value.downcast_ref::<Press>();
            self.armed = true;
            self.payload = press
                .map(|press| press.payload.clone())
                .filter(|text| !text.is_empty());
            self.external = press.is_some_and(|press| press.external);
            return Ok(());
        }
        let Some(text) = value.downcast_ref::<String>() else {
            return Ok(());
        };
        match name {
            "command" => self.command = words("command", text)?,
            "env" => self.env = parse_env(text)?,
            "cwd" => self.cwd = text.clone(),
            "artifacts" => self.artifacts = words("artifacts", text)?,
            "keep_on_failure" => {
                self.keep_on_failure = match text.trim() {
                    "" | "true" => true,
                    "false" => false,
                    other => {
                        return Err(ZeughausError::InvalidParameter(format!(
                            "keep_on_failure: '{other}' is neither true nor false"
                        )));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Splits a setting the way a POSIX shell splits words -- quotes and
/// backslashes group, nothing expands -- so a path with a space is one
/// argument and `$HOME` reaches the program as written. No shell is involved
/// at run time: the program is started directly with these words.
///
/// The settings are single lines because the editor draws them as such; a
/// list per line would need a widget that does not exist yet.
fn words(setting: &str, text: &str) -> Result<Vec<String>> {
    shlex::split(text).ok_or_else(|| {
        ZeughausError::InvalidParameter(format!(
            "{setting}: unbalanced quote or trailing backslash"
        ))
    })
}

/// `KEY=VALUE` words. A word without `=` is refused rather than dropped:
/// silently ignoring it would start the run without the variable it needs and
/// blame the program for it.
fn parse_env(text: &str) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for word in words("env", text)? {
        let Some((key, value)) = word.split_once('=') else {
            return Err(ZeughausError::InvalidParameter(format!(
                "env: '{word}' is not KEY=VALUE"
            )));
        };
        if key.is_empty() {
            return Err(ZeughausError::InvalidParameter(format!(
                "env: '{word}' has no name"
            )));
        }
        env.push((key.to_string(), value.to_string()));
    }
    Ok(env)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Clears the node's live flag however the wait ended, panic included: a flag
/// left set would refuse every later trigger on that node for the life of the
/// process.
struct LiveGuard(Arc<AtomicBool>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Waiting for the run, recording how it ended, and keeping what it declared.
///
/// All of it off the executor thread: a build takes minutes, and the pass that
/// started it has a store to keep draining.
struct RunWork {
    handle: Box<dyn RunHandle>,
    live: Arc<AtomicBool>,
    run_dir: PathBuf,
    /// The directory artifact globs are relative to; `None` is the runner's.
    cwd: Option<PathBuf>,
    artifacts: Vec<String>,
    started: u64,
}

impl AsyncWork for RunWork {
    fn run(self: Box<Self>) -> Result<HashMap<String, Value>> {
        let RunWork {
            handle,
            live,
            run_dir,
            cwd,
            artifacts,
            started,
        } = *self;
        let _guard = LiveGuard(live);

        let exit = handle.wait();
        record_run_end(&run_dir, exit, started, cwd.as_deref(), &artifacts)?;

        let mut outputs: HashMap<String, Value> = HashMap::new();
        // The directory is reported whatever happened: it is where both the
        // log and the failure are.
        outputs.insert(
            "dir".to_string(),
            Value::new(run_dir.to_string_lossy().into_owned()),
        );
        if exit.code == Some(0) {
            outputs.insert("ok".to_string(), Value::new(true));
        } else {
            // A killed run has no code of its own; -1 is the one value that
            // cannot be a process's own success.
            outputs.insert(
                "failed".to_string(),
                Value::new(i64::from(exit.code.unwrap_or(-1))),
            );
        }
        Ok(outputs)
    }
}

/// Writes a finished run's exit record beside its log and keeps what it
/// declared as artifacts: what a later reader needs to say whether the run
/// succeeded without parsing terminal output.
///
/// The node calls it when its wait ends; a runner that adopted a run from a
/// previous process calls it for runs no node waits for any more.
pub fn record_run_end(
    run_dir: &Path,
    exit: RunExit,
    started: u64,
    cwd: Option<&Path>,
    artifacts: &[String],
) -> Result<()> {
    let finished = unix_secs();
    let record = format!(
        "code={}\nkilled={}\nstarted={started}\nfinished={finished}\n",
        match exit.code {
            Some(code) => code.to_string(),
            None => "none".to_string(),
        },
        exit.killed
    );
    let exit_path = run_dir.join("exit");
    std::fs::write(&exit_path, record).map_err(|e| {
        ZeughausError::ExecutionFailed(format!("job: cannot write {}: {e}", exit_path.display()))
    })?;
    copy_artifacts(run_dir, cwd, artifacts)
}

/// Copies every file a declared glob matches into `<run_dir>/artifacts/`,
/// keeping its path relative to the directory the run worked in.
///
/// Directories are skipped rather than walked: a glob that names one matches
/// its contents too when the user writes it that way, and copying a tree the
/// pattern did not ask for is how a run's artifacts become a second copy of
/// the working directory.
fn copy_artifacts(run_dir: &Path, cwd: Option<&Path>, artifacts: &[String]) -> Result<()> {
    if artifacts.is_empty() {
        return Ok(());
    }
    let base = match cwd {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().map_err(|e| {
            ZeughausError::ExecutionFailed(format!("job: no working directory: {e}"))
        })?,
    };
    let target = run_dir.join("artifacts");

    for pattern in artifacts {
        let joined = base.join(pattern);
        let matches = glob::glob(&joined.to_string_lossy()).map_err(|e| {
            ZeughausError::ExecutionFailed(format!("job: artifact glob '{pattern}': {e}"))
        })?;
        for entry in matches {
            let path = entry.map_err(|e| {
                ZeughausError::ExecutionFailed(format!("job: artifact '{pattern}': {e}"))
            })?;
            if !path.is_file() {
                continue;
            }
            let relative = path.strip_prefix(&base).unwrap_or(&path);
            let destination = target.join(relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ZeughausError::ExecutionFailed(format!(
                        "job: cannot create {}: {e}",
                        parent.display()
                    ))
                })?;
            }
            std::fs::copy(&path, &destination).map_err(|e| {
                ZeughausError::ExecutionFailed(format!("job: cannot copy {}: {e}", path.display()))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunExit;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zeughaus-job-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    struct FakeRun {
        exit: RunExit,
    }

    impl RunHandle for FakeRun {
        fn wait(self: Box<Self>) -> RunExit {
            self.exit
        }
    }

    struct FakeHost {
        held: bool,
        exit: RunExit,
        root: PathBuf,
        specs: Mutex<Vec<JobSpec>>,
    }

    impl FakeHost {
        fn new(exit: RunExit) -> Arc<FakeHost> {
            Arc::new(FakeHost {
                held: false,
                exit,
                root: scratch(),
                specs: Mutex::new(Vec::new()),
            })
        }

        fn held() -> Arc<FakeHost> {
            Arc::new(FakeHost {
                held: true,
                exit: RunExit {
                    code: Some(0),
                    killed: false,
                },
                root: scratch(),
                specs: Mutex::new(Vec::new()),
            })
        }

        fn spec(&self) -> JobSpec {
            self.specs.lock().expect("specs")[0].clone()
        }
    }

    impl ProcessHost for FakeHost {
        fn held(&self) -> bool {
            self.held
        }

        fn new_run_dir(&self) -> std::result::Result<PathBuf, String> {
            let n = self.specs.lock().expect("specs").len();
            let dir = self.root.join(n.to_string());
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            Ok(dir)
        }

        fn spawn(&self, spec: JobSpec) -> std::result::Result<Box<dyn RunHandle>, String> {
            self.specs.lock().expect("specs").push(spec);
            Ok(Box::new(FakeRun { exit: self.exit }))
        }
    }

    fn ok_exit() -> RunExit {
        RunExit {
            code: Some(0),
            killed: false,
        }
    }

    fn job(host: &Arc<FakeHost>) -> JobNode {
        let mut node = JobNode::new(Some(Arc::clone(host) as Arc<dyn ProcessHost>));
        node.set_parameter("command", Value::new("/bin/echo".to_string()))
            .expect("command");
        node
    }

    /// Fires the node once as a press does, returning the deferred wait.
    fn fire(node: &mut JobNode) -> Result<Option<Box<dyn AsyncWork>>> {
        node.set_parameter("fire", Value::new(Press::default()))
            .expect("fire");
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&InputSet::new(), &mut ctx)?;
        Ok(ctx.take_deferred())
    }

    /// The refusal a fire produced. `Box<dyn AsyncWork>` is not `Debug`, so
    /// the success side cannot be unwrapped by `expect_err`.
    fn refusal(node: &mut JobNode) -> ZeughausError {
        match fire(node) {
            Ok(_) => panic!("the run was not refused"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_trigger_while_a_run_is_live_is_refused() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        assert!(fire(&mut node).expect("first run").is_some());
        // The first run's wait was never executed, so it is still live.
        let err = refusal(&mut node);
        assert!(format!("{err}").contains("busy"), "{err}");
        assert_eq!(host.specs.lock().expect("specs").len(), 1);
    }

    #[test]
    fn a_held_runner_starts_nothing() {
        let host = FakeHost::held();
        let mut node = job(&host);
        let err = refusal(&mut node);
        assert!(format!("{err}").contains("held"), "{err}");
        assert!(host.specs.lock().expect("specs").is_empty());
    }

    #[test]
    fn a_node_without_a_host_refuses_to_run() {
        let mut node = JobNode::new(None);
        node.set_parameter("command", Value::new("/bin/echo".to_string()))
            .expect("command");
        let err = refusal(&mut node);
        assert!(format!("{err}").contains("does not execute"), "{err}");
    }

    #[test]
    fn an_env_word_without_an_equals_sign_is_refused() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        let err = node
            .set_parameter("env", Value::new("PATH=/bin BROKEN".to_string()))
            .expect_err("env");
        assert!(matches!(err, ZeughausError::InvalidParameter(_)), "{err}");
        // The refused value never took effect.
        assert!(node.env.is_empty());
    }

    #[test]
    fn an_unbalanced_quote_is_refused_and_keeps_the_old_command() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        let err = node
            .set_parameter(
                "command",
                Value::new("cargo test \"unterminated".to_string()),
            )
            .expect_err("command");
        assert!(matches!(err, ZeughausError::InvalidParameter(_)), "{err}");
        assert_eq!(node.command, vec!["/bin/echo".to_string()]);
    }

    #[test]
    fn the_trigger_payload_and_a_wired_cwd_reach_the_run() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        node.set_parameter("cwd", Value::new("/from/setting".to_string()))
            .expect("cwd");
        node.set_parameter(
            "fire",
            Value::new(Press {
                payload: "{\"ref\":\"main\"}".to_string(),
                external: false,
            }),
        )
        .expect("fire");
        let mut inputs = InputSet::new();
        inputs.insert("cwd", Value::new("/from/wire".to_string()));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).expect("run");
        let spec = host.spec();
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/from/wire")));
        assert_eq!(
            spec.env,
            vec![(
                "ZEUGHAUS_PAYLOAD".to_string(),
                "{\"ref\":\"main\"}".to_string()
            )]
        );

        // The payload is spent with the press: a run started by the wire
        // carries the wire's text instead, and none when it is not text.
        ctx.take_deferred().expect("deferred").run().expect("wait");
        let mut inputs = InputSet::new();
        inputs.insert("run", Value::new("sha-123".to_string()));
        inputs.mark_changed("run");
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).expect("run");
        let spec = host.specs.lock().expect("specs")[1].clone();
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/from/setting")));
        assert_eq!(
            spec.env,
            vec![("ZEUGHAUS_PAYLOAD".to_string(), "sha-123".to_string())]
        );
        ctx.take_deferred().expect("deferred").run().expect("wait");
        let mut inputs = InputSet::new();
        inputs.insert("run", Value::new(true));
        inputs.mark_changed("run");
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).expect("run");
        assert!(host.specs.lock().expect("specs")[2].env.is_empty());
    }

    #[test]
    fn only_an_external_press_starts_an_external_run() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        node.set_parameter(
            "fire",
            Value::new(Press {
                payload: String::new(),
                external: true,
            }),
        )
        .expect("fire");
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&InputSet::new(), &mut ctx).expect("run");
        assert!(host.spec().external);
        ctx.take_deferred().expect("deferred").run().expect("wait");

        // The flag is spent with the press: the `run` pin starts a run of
        // its own, and so does an editor's press.
        let mut inputs = InputSet::new();
        inputs.insert("run", Value::new(true));
        inputs.mark_changed("run");
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).expect("run");
        ctx.take_deferred().expect("deferred").run().expect("wait");
        assert!(fire(&mut node).expect("run").is_some());
        let specs = host.specs.lock().expect("specs");
        assert_eq!(
            specs.iter().map(|spec| spec.external).collect::<Vec<_>>(),
            [true, false, false]
        );
    }

    #[test]
    fn the_spec_comes_out_of_the_settings() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        node.set_parameter(
            "command",
            Value::new("/bin/echo -n 'hello world' \\$HOME".to_string()),
        )
        .expect("command");
        node.set_parameter("env", Value::new("A=1 B=two=three C=\"x y\"".to_string()))
            .expect("env");
        node.set_parameter("cwd", Value::new("   ".to_string()))
            .expect("cwd");
        assert!(fire(&mut node).expect("run").is_some());

        let spec = host.spec();
        assert_eq!(spec.label, "echo");
        assert_eq!(spec.program, "/bin/echo");
        // Quotes group, nothing expands: no shell stands between the setting
        // and the program.
        assert_eq!(
            spec.args,
            vec![
                "-n".to_string(),
                "hello world".to_string(),
                "$HOME".to_string()
            ]
        );
        assert_eq!(
            spec.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two=three".to_string()),
                ("C".to_string(), "x y".to_string()),
            ]
        );
        // Blank is the runner's own directory, not a directory named "".
        assert_eq!(spec.cwd, None);
    }

    #[test]
    fn a_zero_exit_is_ok_and_anything_else_is_failed() {
        let host = FakeHost::new(ok_exit());
        let mut node = job(&host);
        let work = fire(&mut node).expect("run").expect("deferred");
        let outputs = work.run().expect("wait");
        assert_eq!(
            outputs.get("ok").and_then(|v| v.downcast_ref::<bool>()),
            Some(&true)
        );
        assert!(!outputs.contains_key("failed"));
        let run_dir = outputs
            .get("dir")
            .and_then(|v| v.downcast_ref::<String>())
            .expect("run dir")
            .clone();
        let record = std::fs::read_to_string(PathBuf::from(&run_dir).join("exit")).expect("exit");
        assert!(record.starts_with("code=0\nkilled=false\n"), "{record}");
        // The run is no longer live, so the node fires again.
        assert!(fire(&mut node).expect("second run").is_some());

        let host = FakeHost::new(RunExit {
            code: Some(2),
            killed: false,
        });
        let mut node = job(&host);
        let work = fire(&mut node).expect("run").expect("deferred");
        let outputs = work.run().expect("wait");
        assert_eq!(
            outputs.get("failed").and_then(|v| v.downcast_ref::<i64>()),
            Some(&2)
        );
        assert!(!outputs.contains_key("ok"));
    }

    #[test]
    fn a_killed_run_fails_with_minus_one() {
        let host = FakeHost::new(RunExit {
            code: None,
            killed: true,
        });
        let mut node = job(&host);
        let work = fire(&mut node).expect("run").expect("deferred");
        let outputs = work.run().expect("wait");
        assert_eq!(
            outputs.get("failed").and_then(|v| v.downcast_ref::<i64>()),
            Some(&-1)
        );
        let run_dir = outputs
            .get("dir")
            .and_then(|v| v.downcast_ref::<String>())
            .expect("run dir")
            .clone();
        let record = std::fs::read_to_string(PathBuf::from(&run_dir).join("exit")).expect("exit");
        assert!(record.starts_with("code=none\nkilled=true\n"), "{record}");
    }

    #[test]
    fn declared_artifacts_are_kept_beside_the_log() {
        let host = FakeHost::new(ok_exit());
        let work_dir = scratch().join("work");
        std::fs::create_dir_all(work_dir.join("out")).expect("work dir");
        std::fs::write(work_dir.join("out/report.txt"), b"result").expect("artifact");

        let mut node = job(&host);
        node.set_parameter("cwd", Value::new(work_dir.to_string_lossy().into_owned()))
            .expect("cwd");
        node.set_parameter("artifacts", Value::new("out/*.txt".to_string()))
            .expect("artifacts");
        let work = fire(&mut node).expect("run").expect("deferred");
        let outputs = work.run().expect("wait");

        let run_dir = PathBuf::from(
            outputs
                .get("dir")
                .and_then(|v| v.downcast_ref::<String>())
                .expect("run dir"),
        );
        let kept =
            std::fs::read_to_string(run_dir.join("artifacts/out/report.txt")).expect("artifact");
        assert_eq!(kept, "result");
    }
}
