//! Periodic maintenance of the host this runner lives on.
//!
//! `<state-dir>/maintain.toml` lists tasks, each an argv that runs every so
//! many minutes:
//!
//! ```toml
//! [[task]]
//! name = "rust"
//! command = ["rustup", "update"]
//! every_minutes = 60
//! timeout_minutes = 30     # the default
//! ```
//!
//! One thread reads the file once a minute (a symlink into a dotfiles
//! checkout is fine, and an edit needs no restart), and runs a due task as a
//! run of the job host: a terminal in the `Triggered` group with a WAL under
//! `<state-dir>/runs/<id>`, kept for attaching when it fails. Tasks run one
//! at a time, and only when this runner is idle: while one runs the host
//! counts as held, so CI prepares wait and graph jobs are refused, and a
//! toolchain update never swaps files under a build.
//!
//! `<state-dir>/maintain/<name>.json` records the last start of each task;
//! it is what makes a task due again, and what `zeughaus-runner maintain`
//! prints. A failed task sends an [`Alert`], which the runner publishes to
//! every editor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zeughaus_job::{JobSpec, ProcessHost, RunHandle};

use crate::files;
use crate::jobs::JobHost;
use crate::names::valid_name;

/// `<state-dir>/maintain.toml`.
const CONFIG_FILE: &str = "maintain.toml";

/// `<state-dir>/maintain/`: one record per task.
const STATE_DIR: &str = "maintain";

/// The wait before the first look, so that a runner that has just started or
/// restarted is serving before it begins a task.
const FIRST_LOOK: Duration = Duration::from_secs(30);

/// The time between two looks at the file.
const INTERVAL: Duration = Duration::from_secs(60);

/// What `timeout` answers when its deadline ended the run.
const TIMED_OUT: i32 = 124;

/// A week: the longest period a task may have.
const MAX_EVERY_MINUTES: u32 = 10_080;

/// A day: the longest deadline a task may have.
const MAX_TIMEOUT_MINUTES: u32 = 1_440;

fn thirty() -> u32 {
    30
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintainFile {
    #[serde(default)]
    task: Vec<Task>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Task {
    /// A file name under `maintain/` and the label of the run's terminal.
    name: String,
    /// The program and its arguments. No shell: a task that needs one says
    /// so in its argv.
    command: Vec<String>,
    every_minutes: u32,
    #[serde(default = "thirty")]
    timeout_minutes: u32,
}

/// What a failed task asks the runner to tell the editors.
pub struct Alert {
    pub title: String,
    pub body: String,
}

/// The last start of a task. Written when it starts and again when it ends,
/// so a record without `finished` is a task that is running, or that was cut
/// short with the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    /// Unix seconds.
    started: u64,
    #[serde(default)]
    finished: Option<u64>,
    #[serde(default)]
    code: Option<i32>,
    #[serde(default)]
    killed: bool,
    /// The run directory holding the task's `wal`; `None` when the task never
    /// started.
    #[serde(default)]
    run: Option<String>,
}

/// How a finished task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    Exit(i32),
    TimedOut,
    Killed,
    /// A signal ended it, or it never started.
    NoCode,
}

impl Outcome {
    fn of(code: Option<i32>, killed: bool) -> Outcome {
        match (code, killed) {
            (_, true) => Outcome::Killed,
            (Some(0), false) => Outcome::Ok,
            (Some(TIMED_OUT), false) => Outcome::TimedOut,
            (Some(n), false) => Outcome::Exit(n),
            (None, false) => Outcome::NoCode,
        }
    }

    /// The word of a log line and an alert.
    fn reason(self) -> String {
        match self {
            Outcome::Ok => "ok".to_owned(),
            Outcome::Exit(n) => format!("exited {n}"),
            Outcome::TimedOut => "timed out".to_owned(),
            Outcome::Killed => "killed".to_owned(),
            Outcome::NoCode => "ended without an exit code".to_owned(),
        }
    }

    /// The column of `zeughaus-runner maintain`.
    fn label(self) -> String {
        match self {
            Outcome::Exit(n) => format!("exit {n}"),
            Outcome::NoCode => "no exit code".to_owned(),
            other => other.reason(),
        }
    }
}

impl State {
    fn outcome(&self) -> Option<Outcome> {
        self.finished?;
        Some(Outcome::of(self.code, self.killed))
    }
}

fn parse(text: &str) -> Result<Vec<Task>, String> {
    let file: MaintainFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = std::collections::HashSet::new();
    for task in &file.task {
        if !valid_name(&task.name) {
            return Err(format!(
                "task name `{}` must match [a-z0-9][a-z0-9-]*",
                task.name
            ));
        }
        if !seen.insert(task.name.as_str()) {
            return Err(format!("task `{}` is listed twice", task.name));
        }
        if task
            .command
            .first()
            .is_none_or(|program| program.is_empty())
        {
            return Err(format!("task `{}`: command is empty", task.name));
        }
        if !(1..=MAX_EVERY_MINUTES).contains(&task.every_minutes) {
            return Err(format!(
                "task `{}`: every_minutes must be 1..={MAX_EVERY_MINUTES}",
                task.name
            ));
        }
        if !(1..=MAX_TIMEOUT_MINUTES).contains(&task.timeout_minutes) {
            return Err(format!(
                "task `{}`: timeout_minutes must be 1..={MAX_TIMEOUT_MINUTES}",
                task.name
            ));
        }
    }
    Ok(file.task)
}

/// `Ok(None)` when there is no `maintain.toml`: this runner has nothing to
/// maintain. An error names the file.
fn load(state_dir: &Path) -> Result<Option<Vec<Task>>, String> {
    let path = state_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn state_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(STATE_DIR).join(format!("{name}.json"))
}

/// The record of `name`. An unreadable one is reported and counts as none:
/// the task runs again, which rewrites it.
fn load_state(state_dir: &Path, name: &str) -> Option<State> {
    let path = state_path(state_dir, name);
    if !path.exists() {
        return None;
    }
    match files::read_json(&path) {
        Ok(state) => Some(state),
        Err(e) => {
            eprintln!("[maintain] {e}");
            None
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Whether a task last started as `state` says is due at `now`.
fn due(state: Option<&State>, every_minutes: u32, now: u64) -> bool {
    state.is_none_or(|s| now >= s.started.saturating_add(u64::from(every_minutes) * 60))
}

/// Starts the maintenance thread. `exe` is this binary: the tasks run under
/// its `timeout` subcommand, which macOS and Windows have no equivalent of.
/// Failures leave through `alerts`; the runner's loop publishes them.
pub fn start(host: Arc<JobHost>, state_dir: PathBuf, exe: PathBuf, alerts: Sender<Alert>) {
    let spawned = std::thread::Builder::new()
        .name("zeughaus-maintain".to_owned())
        .spawn(move || {
            std::thread::sleep(FIRST_LOOK);
            // The last file error logged: it is read every minute, and a
            // broken file says so once.
            let mut logged = None;
            loop {
                tick(&host, &state_dir, &exe, &alerts, &mut logged);
                std::thread::sleep(INTERVAL);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[maintain] cannot start the maintenance thread: {e}");
    }
}

fn tick(
    host: &JobHost,
    state_dir: &Path,
    exe: &Path,
    alerts: &Sender<Alert>,
    logged: &mut Option<String>,
) {
    let tasks = match load(state_dir) {
        Ok(Some(tasks)) => {
            *logged = None;
            tasks
        }
        Ok(None) => return,
        Err(e) => {
            if logged.as_deref() != Some(e.as_str()) {
                eprintln!("[maintain] {e}");
                *logged = Some(e);
            }
            return;
        }
    };
    for task in &tasks {
        let state = load_state(state_dir, &task.name);
        if !due(state.as_ref(), task.every_minutes, now()) {
            continue;
        }
        // Not now: a held runner is draining or paused by hand, and a live
        // run is somebody's build. The task stays due for the next look.
        if host.held() || host.live_runs() != 0 {
            return;
        }
        run_task(host, state_dir, exe, task, alerts);
    }
}

fn run_task(host: &JobHost, state_dir: &Path, exe: &Path, task: &Task, alerts: &Sender<Alert>) {
    let path = state_path(state_dir, &task.name);
    let mut state = State {
        started: now(),
        finished: None,
        code: None,
        killed: false,
        run: None,
    };
    host.set_maintaining(true);
    let outcome = match start_run(host, exe, task, &mut state, &path) {
        Ok(handle) => {
            let exit = handle.wait();
            state.code = exit.code;
            state.killed = exit.killed;
            Outcome::of(exit.code, exit.killed)
        }
        Err(e) => {
            eprintln!("[maintain] {}: cannot start: {e}", task.name);
            Outcome::NoCode
        }
    };
    host.set_maintaining(false);
    state.finished = Some(now());
    if let Err(e) = files::write_json(&path, &state) {
        eprintln!("[maintain] {}: {e}", task.name);
    }
    if outcome == Outcome::Ok {
        eprintln!("[maintain] {}: ok", task.name);
        return;
    }
    let log = state
        .run
        .as_deref()
        .map(|run| format!("; log {}", Path::new(run).join("wal").display()))
        .unwrap_or_default();
    let line = format!("{}{log}", outcome.reason());
    eprintln!("[maintain] {}: {line}", task.name);
    let alert = Alert {
        title: format!("maintenance failed: {}", task.name),
        body: format!("{}: {line}", hostname()),
    };
    if alerts.send(alert).is_err() {
        eprintln!("[maintain] {}: nobody to alert", task.name);
    }
}

/// Records the start and spawns the task. `state.run` is set before the
/// record is written, so a runner that dies mid-task leaves the log's place.
fn start_run(
    host: &JobHost,
    exe: &Path,
    task: &Task,
    state: &mut State,
    path: &Path,
) -> Result<Box<dyn RunHandle>, String> {
    let run_dir = host.new_run_dir()?;
    state.run = Some(run_dir.to_string_lossy().into_owned());
    files::write_json(path, &*state)?;
    let mut args = vec![
        "timeout".to_owned(),
        (u64::from(task.timeout_minutes) * 60).to_string(),
        "--".to_owned(),
    ];
    args.extend(task.command.iter().cloned());
    host.spawn(JobSpec {
        label: format!("maintain {}", task.name),
        program: exe.to_string_lossy().into_owned(),
        args,
        env: vec![("ZEUGHAUS_MAINTAIN".to_owned(), task.name.clone())],
        cwd: home(),
        run_dir,
        keep_on_failure: true,
        started: state.started,
        artifacts: Vec::new(),
        external: true,
    })
}

fn home() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown host".to_owned())
}

fn age(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s} s"),
        s if s < 3_600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{} h", s / 3_600),
        s => format!("{} d", s / 86_400),
    }
}

/// What `zeughaus-runner maintain` prints: one line per task. An error is a
/// `maintain.toml` that cannot be read or does not hold.
pub fn report(state_dir: &Path) -> Result<String, String> {
    report_at(state_dir, now())
}

fn report_at(state_dir: &Path, now: u64) -> Result<String, String> {
    let Some(tasks) = load(state_dir)? else {
        return Ok(format!(
            "no tasks: {} does not exist",
            state_dir.join(CONFIG_FILE).display()
        ));
    };
    let mut lines = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let mut line = format!("{}  every {} min  ", task.name, task.every_minutes);
        match load_state(state_dir, &task.name) {
            None => line.push_str("never"),
            Some(state) => {
                let status = state
                    .outcome()
                    .map_or_else(|| "running".to_owned(), Outcome::label);
                line.push_str(&format!(
                    "{status}  {} ago",
                    age(now.saturating_sub(state.started))
                ));
                if let Some(run) = &state.run {
                    line.push_str(&format!("  {run}"));
                }
            }
        }
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(started: u64) -> State {
        State {
            started,
            finished: Some(started + 5),
            code: Some(0),
            killed: false,
            run: Some("/runs/7".to_owned()),
        }
    }

    #[test]
    fn a_task_has_a_default_deadline() {
        let tasks = parse(
            "[[task]]\nname = \"rust\"\ncommand = [\"rustup\", \"update\"]\nevery_minutes = 60\n",
        )
        .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].timeout_minutes, 30);
    }

    #[test]
    fn an_empty_command_is_refused() {
        let err =
            parse("[[task]]\nname = \"rust\"\ncommand = []\nevery_minutes = 60\n").unwrap_err();
        assert!(err.contains("command is empty"), "{err}");
    }

    #[test]
    fn a_duplicate_name_is_refused() {
        let text = "[[task]]\nname = \"rust\"\ncommand = [\"a\"]\nevery_minutes = 60\n\
                    [[task]]\nname = \"rust\"\ncommand = [\"b\"]\nevery_minutes = 60\n";
        let err = parse(text).unwrap_err();
        assert!(err.contains("listed twice"), "{err}");
    }

    #[test]
    fn names_and_periods_are_bounded() {
        let bad_name = "[[task]]\nname = \"../x\"\ncommand = [\"a\"]\nevery_minutes = 1\n";
        assert!(parse(bad_name).is_err());
        let no_period = "[[task]]\nname = \"x\"\ncommand = [\"a\"]\nevery_minutes = 0\n";
        assert!(parse(no_period).is_err());
        let long_deadline = "[[task]]\nname = \"x\"\ncommand = [\"a\"]\nevery_minutes = 1\ntimeout_minutes = 1441\n";
        assert!(parse(long_deadline).is_err());
        assert!(
            parse("[[task]]\nname = \"x\"\ncommand = [\"a\"]\nevery_minutes = 1\nbogus = 1\n")
                .is_err()
        );
        assert_eq!(parse("").unwrap(), Vec::new());
    }

    #[test]
    fn a_task_is_due_by_its_last_start() {
        let every = 10;
        // Never run.
        assert!(due(None, every, 1_000_000));
        // The next start is still ahead.
        assert!(!due(Some(&state(1_000_000)), every, 1_000_000 + 599));
        // Exactly at, and past, the period.
        assert!(due(Some(&state(1_000_000)), every, 1_000_000 + 600));
        assert!(due(Some(&state(1_000_000)), every, 1_000_000 + 5_000));
    }

    #[test]
    fn outcomes_are_told_apart() {
        assert_eq!(Outcome::of(Some(0), false), Outcome::Ok);
        assert_eq!(Outcome::of(Some(3), false), Outcome::Exit(3));
        assert_eq!(Outcome::of(Some(124), false), Outcome::TimedOut);
        assert_eq!(Outcome::of(None, true), Outcome::Killed);
        assert_eq!(Outcome::of(None, false), Outcome::NoCode);
        assert_eq!(Outcome::Exit(3).reason(), "exited 3");
        assert_eq!(Outcome::Exit(3).label(), "exit 3");
    }

    #[test]
    fn the_report_names_every_task() {
        let dir = std::env::temp_dir().join(format!("zeughaus-maintain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(report_at(&dir, 0).unwrap().starts_with("no tasks"));
        std::fs::write(
            dir.join(CONFIG_FILE),
            "[[task]]\nname = \"ok\"\ncommand = [\"a\"]\nevery_minutes = 5\n\
             [[task]]\nname = \"bad\"\ncommand = [\"b\"]\nevery_minutes = 5\n\
             [[task]]\nname = \"new\"\ncommand = [\"c\"]\nevery_minutes = 5\n",
        )
        .unwrap();
        files::write_json(&state_path(&dir, "ok"), &state(1_000)).unwrap();
        let mut failed = state(1_000);
        failed.code = Some(3);
        files::write_json(&state_path(&dir, "bad"), &failed).unwrap();
        let report = report_at(&dir, 1_000 + 7_300).unwrap();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[0], "ok  every 5 min  ok  2 h ago  /runs/7");
        assert_eq!(lines[1], "bad  every 5 min  exit 3  2 h ago  /runs/7");
        assert_eq!(lines[2], "new  every 5 min  never");
        std::fs::write(dir.join(CONFIG_FILE), "[[task]]\nname = \"x\"\n").unwrap();
        assert!(report_at(&dir, 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
