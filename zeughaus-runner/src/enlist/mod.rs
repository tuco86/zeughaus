//! A runner enlisted into a CI: the executor half of the enlist protocol.
//!
//! The CI (`zeughaus-ci`) decides which job runs where; this runner runs what
//! it is assigned, as jobs of its own [`JobHost`](crate::jobs::JobHost), and
//! streams each run's log to the CI as it is written. Everything a run needs
//! to survive a restart of this process or of the connection is on disk
//! under `<state-dir>/ci/`: the assignment of every started run
//! (`assigned/<run>.json`), its artifacts, the mirrors and the workspaces.
//!
//! Artifacts are kept once, on the runner that produced them
//! (`artifacts/<run>`, moved out of the run's directory), until the CI
//! sends `DropArtifacts`; a consumer on another runner gets them relayed
//! through the CI, which stores nothing. A consumer's copy of its inputs
//! (`inputs/` in the run directory, the guest's copy on a VM) goes with its
//! run.
//!
//! The connection is the runner's to keep: it dials the CI, says who it is and
//! which runs it still knows, and redials with a growing delay when it is
//! lost. What was not delivered meanwhile is delivered after the next
//! welcome, from the offsets the CI reports it holds.
//!
//! * `config`: `enlist.toml`.
//! * `link`: the connection and the dispatch of what the CI sends.
//! * `run`: one assignment from staging to its end, the log delivery and
//!   the restart of runs a previous process left.
//! * `control`: the clock of this runner: busy state, freezing, machine
//!   status and the VM timeout.
//! * `artifacts`: the artifact sets that cross to and from the CI, streamed.
//! * `prefetch`: the sets fetched before their job is assigned.
//! * `launch`, `machine`, `busy`, `git`, `cleanup`, `stage`: what a run is
//!   rendered from and housekeeping.

pub mod artifacts;
pub mod busy;
pub mod config;

mod cleanup;
mod control;
mod git;
mod launch;
mod link;
mod machine;
mod prefetch;
mod run;
mod stage;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use weida::{ClientTls, ReconnectPolicy, Runtime, RuntimeConfig, Trust};
use zeughaus_link::credentials;
use zeughaus_link::enlist::{Assignment, FromRunner, Offer, RunEnd};

use crate::jobs::JobHost;

use self::busy::Busy;
use self::config::{EnlistConfig, MachineConfig};
use self::machine::Vm;

/// The folder of a repository that holds its CI jobs.
pub const CI_DIR: &str = ".zeughaus-ci";

/// Seconds since the unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The `ci/` directory under the state directory.
pub fn ci_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("ci")
}

/// The assignments of started runs, one `<run>.json` each.
fn assigned_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("assigned")
}

/// The artifact sets of finished runs, one directory per run id. A set
/// lives here, and only here, until the CI drops it.
pub fn artifacts_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("artifacts")
}

fn mirror_dir(state_dir: &Path, repo: &str) -> PathBuf {
    ci_dir(state_dir)
        .join("mirrors")
        .join(format!("{repo}.git"))
}

/// A workspace's directory: `workspace` is `Place::workspace`, the one a
/// repository's jobs at a place share on this runner.
fn work_dir(state_dir: &Path, repo: &str, workspace: &str) -> PathBuf {
    ci_dir(state_dir).join("work").join(repo).join(workspace)
}

/// Scratch space: secrets where there is no tmpfs.
fn tmp_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("tmp")
}

/// What the rest of the runner shares with the executor.
pub struct Enlisted {
    /// The busy measurement, when `enlist.toml` has a `[busy]` section: the
    /// runner serves it on `/busy` and reports it to its editors.
    pub busy: Option<Arc<Busy>>,
}

/// The queue to the CI of the connection that is up, and what the CI said it
/// already holds of each run's log.
#[derive(Default)]
struct Link {
    /// Counts connections, so a thread that remembers what it sent on one
    /// knows when a new one makes that stale.
    generation: u64,
    tx: Option<mpsc::Sender<FromRunner>>,
    offsets: HashMap<String, u64>,
}

/// What this process knows of one assigned run.
pub struct RunCtl {
    /// As persisted: without secrets and the fetch token.
    pub assignment: Assignment,
    /// The CI asked for the run to end before it started, or after.
    pub abort: AtomicBool,
    /// The CI is done with the run; its delivery thread ends.
    pub forgotten: AtomicBool,
    /// Paused because the machine got busy.
    pub frozen: AtomicBool,
    /// A failed freeze or thaw was said once.
    pub freeze_warned: AtomicBool,
    /// Milliseconds a machine job has run unfrozen: the clock its timeout
    /// follows.
    pub active_millis: AtomicU64,
    /// The run directory, once the process runs.
    pub live: Mutex<Option<PathBuf>>,
    /// How it ended, once it did and its artifacts are kept.
    pub end: Mutex<Option<RunEnd>>,
}

impl RunCtl {
    fn new(assignment: Assignment) -> RunCtl {
        RunCtl {
            assignment,
            abort: Default::default(),
            forgotten: Default::default(),
            frozen: Default::default(),
            freeze_warned: Default::default(),
            active_millis: Default::default(),
            live: Mutex::new(None),
            end: Mutex::new(None),
        }
    }

    /// The run directory while the process runs: set, and no end yet.
    pub fn running_in(&self) -> Option<PathBuf> {
        if lock(&self.end).is_some() {
            return None;
        }
        lock(&self.live).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The executor: everything the connection, the runs and the clock share.
pub struct Executor {
    host: Arc<JobHost>,
    state_dir: PathBuf,
    /// This binary, which a host job's `inner.sh` runs `timeout` through.
    exe: PathBuf,
    config: EnlistConfig,
    /// `CI_CPUS` of a host job.
    cpus: u32,
    /// Whether `[containers]` is configured and the host's container engine
    /// answers.
    containers: bool,
    machines: BTreeMap<String, Arc<Vm>>,
    busy: Option<Arc<Busy>>,
    runtime: Runtime,
    tls: ClientTls,
    /// Runs the async parts (artifact transfers) for the threads that
    /// execute a run.
    handle: tokio::runtime::Handle,
    runs: Mutex<HashMap<String, Arc<RunCtl>>>,
    link: Mutex<Link>,
    /// One lock per repository's mirror.
    mirrors: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The sets fetched for jobs that are not assigned yet.
    prefetches: prefetch::Prefetches,
}

/// Starts enlisting: the executor's threads and the connection task. Called
/// inside the runner's tokio runtime.
pub fn start(
    host: Arc<JobHost>,
    config: EnlistConfig,
    state_dir: &Path,
    exe: PathBuf,
) -> Result<Enlisted, String> {
    let identity = credentials::runner_identity(state_dir)?;
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|e| format!("enlist needs the runner's async runtime: {e}"))?;
    let runtime = Runtime::new(RuntimeConfig {
        reconnect: ReconnectPolicy {
            initial: std::time::Duration::from_secs(1),
            max: std::time::Duration::from_secs(30),
            ..ReconnectPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .map_err(|e| format!("enlist: weida: {e}"))?;
    let tls = ClientTls::new(Trust::by_address()).with_identity(identity);

    let engine = stage::Engine::host();
    let containers = config.containers.is_some() && stage::engine(engine, &["--version"]).is_ok();
    if config.containers.is_some() && !containers {
        eprintln!(
            "[enlist] [containers] is configured but {} does not answer; offering none",
            engine.program()
        );
    }
    let machines: BTreeMap<String, Arc<Vm>> = config
        .machines
        .iter()
        .map(|(name, machine)| {
            let MachineConfig::WindowsVm(vm) = machine;
            (name.clone(), Vm::new(name, vm.clone(), state_dir))
        })
        .collect();
    let busy = config
        .busy
        .clone()
        .map(|busy| self::busy::start(busy, state_dir));
    let cpus = config.cpus.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(1))
    });

    let executor = Arc::new(Executor {
        host,
        state_dir: state_dir.to_owned(),
        exe,
        config,
        cpus,
        containers,
        machines,
        busy: busy.clone(),
        runtime,
        tls,
        handle,
        runs: Mutex::new(HashMap::new()),
        link: Mutex::new(Link::default()),
        mirrors: Mutex::new(HashMap::new()),
        prefetches: prefetch::Prefetches::default(),
    });
    if executor.containers || !executor.machines.is_empty() {
        control::thaw_everything(&executor);
    }
    // An earlier process fetched sets early for jobs it knew of; a job
    // assigned now fetches its inputs itself.
    if let Err(e) = cleanup::remove_tree(&prefetch::prefetch_dir(state_dir)) {
        eprintln!("[enlist] cannot empty the early fetches: {e}");
    }
    executor.recover();
    executor.sweep_inputs();
    control::spawn(&executor);
    tokio::spawn(link::keep(Arc::clone(&executor)));
    Ok(Enlisted { busy })
}

impl Executor {
    fn offer(&self) -> Offer {
        Offer {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            cpus: self.cpus,
            containers: self.containers,
            machines: self.machines.keys().cloned().collect(),
        }
    }

    fn ctl(&self, run: &str) -> Option<Arc<RunCtl>> {
        lock(&self.runs).get(run).cloned()
    }

    /// The runs this process started, for `Hello`. A run that has ended is
    /// reported as still known and without its end: its log is delivered
    /// after the welcome and the end follows it, so the CI never reads the
    /// end of a run whose log it does not have yet.
    fn reports(&self) -> Vec<zeughaus_link::enlist::RunReport> {
        lock(&self.runs)
            .iter()
            .filter(|(_, ctl)| lock(&ctl.live).is_some())
            .map(|(run, _)| zeughaus_link::enlist::RunReport {
                run: run.clone(),
                end: None,
            })
            .collect()
    }

    /// Queues `message` for the CI. False when no connection is up or it
    /// ended meanwhile, in which case the message is dropped: what matters is
    /// re-sent after the next welcome. Blocks while the connection's queue is
    /// full, so it must not be called from an async task.
    fn send(&self, message: FromRunner) -> bool {
        let tx = lock(&self.link).tx.clone();
        tx.is_some_and(|tx| tx.blocking_send(message).is_ok())
    }

    /// The connection that is up: its generation, its queue and how much of
    /// `run`'s log the CI holds.
    fn link_of(&self, run: &str) -> Option<(u64, mpsc::Sender<FromRunner>, u64)> {
        let link = lock(&self.link);
        let tx = link.tx.clone().filter(|tx| !tx.is_closed())?;
        Some((
            link.generation,
            tx,
            link.offsets.get(run).copied().unwrap_or(0),
        ))
    }

    fn generation(&self) -> Option<u64> {
        let link = lock(&self.link);
        link.tx
            .as_ref()
            .filter(|tx| !tx.is_closed())
            .map(|_| link.generation)
    }
}
