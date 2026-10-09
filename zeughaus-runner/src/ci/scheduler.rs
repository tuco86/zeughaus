//! The scheduler thread: queued events become pipelines, pipelines become
//! job runs on the [`JobHost`].
//!
//! Every decision is persisted in `ci/pipelines/<repo>/<n>.json` before it
//! is acted on, so a runner that restarts (`SIGUSR1` replaces the process,
//! threads and all) finds its pipelines on disk. The runs themselves live in
//! terminals that survive the restart; their end is read back from the exit
//! record [`JobHost::adopt_runs`] writes.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeughaus_job::{JobSpec, ProcessHost, RunExit, record_run_end};

use super::busy::{self, Busy};
use super::config::{CiConfig, RepoConfig, read_secret};
use super::event::{self, CiEvent, EventKind};
use super::forge::{self, State, Status};
use super::header::{JobDef, WhenBusy};
use super::launch::{self, Launch, Place};
use super::machine::Machine;
use super::{CI_DIR, cleanup, pipeline};
use crate::jobs::JobHost;

const TICK: Duration = Duration::from_secs(1);
/// How often a repository's cron schedules are re-read without a push.
const CRON_REFRESH: Duration = Duration::from_secs(3600);
/// How often a recovered run's exit record is looked for.
const RECOVERY_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    /// Pending, held back because the machine is busy.
    Waiting,
    Starting,
    Running,
    /// Running, paused because the machine is busy.
    Frozen,
    Succeeded,
    Failed,
    Skipped,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Succeeded | JobStatus::Failed | JobStatus::Skipped
        )
    }

    fn is_active(self) -> bool {
        matches!(
            self,
            JobStatus::Starting | JobStatus::Running | JobStatus::Frozen
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Waiting => "waiting",
            JobStatus::Starting => "starting",
            JobStatus::Running => "running",
            JobStatus::Frozen => "frozen",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStatus {
    Running,
    Succeeded,
    Failed,
    /// The pipeline could not be set up: fetch, resolve or `.zeughaus-ci` validation.
    Error,
}

impl PipelineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PipelineStatus::Running => "running",
            PipelineStatus::Succeeded => "succeeded",
            PipelineStatus::Failed => "failed",
            PipelineStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub def: JobDef,
    pub status: JobStatus,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub run_dir: Option<PathBuf>,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub started: Option<u64>,
    #[serde(default)]
    pub finished: Option<u64>,
    /// The container image a container job ran in.
    #[serde(default)]
    pub image_tag: Option<String>,
    /// Seconds a machine job has run unfrozen: its timeout clock.
    #[serde(default)]
    pub active_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub repo: String,
    pub number: u64,
    pub key: String,
    pub event: CiEvent,
    pub sha: Option<String>,
    pub status: PipelineStatus,
    #[serde(default)]
    pub note: String,
    pub created: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    pub jobs: Vec<JobRecord>,
}

fn pipelines_dir(state_dir: &Path, repo: &str) -> PathBuf {
    super::ci_dir(state_dir).join("pipelines").join(repo)
}

fn artifacts_dir(state_dir: &Path, repo: &str, number: u64) -> PathBuf {
    pipelines_dir(state_dir, repo).join(format!("{number}.artifacts"))
}

fn mirror_dir(state_dir: &Path, repo: &str) -> PathBuf {
    super::ci_dir(state_dir)
        .join("mirrors")
        .join(format!("{repo}.git"))
}

fn workspace_dir(state_dir: &Path, repo: &str, job: &str) -> PathBuf {
    super::ci_dir(state_dir).join("work").join(repo).join(job)
}

impl Pipeline {
    fn path(&self, state_dir: &Path) -> PathBuf {
        pipelines_dir(state_dir, &self.repo).join(format!("{}.json", self.number))
    }

    fn save(&self, state_dir: &Path) {
        if let Err(e) = super::write_json(&self.path(state_dir), self) {
            eprintln!(
                "[ci] cannot save pipeline {} #{}: {e}",
                self.repo, self.number
            );
        }
    }
}

/// Every pipeline record on disk, newest first.
pub fn load_pipelines(state_dir: &Path) -> Vec<Pipeline> {
    let root = super::ci_dir(state_dir).join("pipelines");
    let mut out = Vec::new();
    let Ok(repos) = std::fs::read_dir(&root) else {
        return out;
    };
    for repo in repos.flatten() {
        let Ok(files) = std::fs::read_dir(repo.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Ok(pipeline) = super::read_json::<Pipeline>(&path)
            {
                out.push(pipeline);
            }
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse((p.created, p.number)));
    out
}

fn next_number(state_dir: &Path, repo: &str) -> u64 {
    std::fs::read_dir(pipelines_dir(state_dir, repo))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()?
                        .strip_suffix(".json")?
                        .parse::<u64>()
                        .ok()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        + 1
}

/// What a job's thread reports back.
enum Msg {
    /// A machine job's run started, after the machine came up.
    Spawned {
        repo: String,
        number: u64,
        job: String,
    },
    Done {
        repo: String,
        number: u64,
        job: String,
        exit: RunExit,
    },
    /// The job never ran: its machine did not come up or the spawn failed.
    StartFailed {
        repo: String,
        number: u64,
        job: String,
        note: String,
    },
}

struct CronCache {
    schedules: Vec<String>,
    refreshed: Option<Instant>,
}

struct Scheduler {
    host: Arc<JobHost>,
    state_dir: PathBuf,
    config: CiConfig,
    running: Vec<Pipeline>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    busy: Arc<Busy>,
    machines: BTreeMap<String, Arc<Machine>>,
    statuses: Sender<Status>,
    crons: BTreeMap<String, CronCache>,
    last_cron_check: DateTime<Local>,
    last_tick: Instant,
    /// Jobs whose freeze or thaw failed, so it is logged once.
    freeze_warned: HashSet<(String, u64, String)>,
}

/// Starts the CI scheduler thread, unless there is no `ci.toml`. Returns
/// the machine's busy state, which `/busy` overrides and editors are told
/// about.
pub fn start(host: Arc<JobHost>, state_dir: PathBuf) -> Option<Arc<Busy>> {
    let config = match CiConfig::load(&state_dir) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!("[ci] no ci.toml; CI is off");
            return None;
        }
        Err(e) => {
            eprintln!("[ci] {e}; CI is off");
            return None;
        }
    };
    eprintln!(
        "[ci] {} repos, {} machines",
        config.repos.len(),
        config.machines.len()
    );
    let busy = busy::start(config.busy.clone(), &state_dir);
    let shared = Arc::clone(&busy);
    let spawned = std::thread::Builder::new()
        .name("zeughaus-ci".into())
        .spawn(move || {
            let mut scheduler = Scheduler::new(host, state_dir, config, shared);
            scheduler.recover();
            loop {
                scheduler.tick();
                std::thread::sleep(TICK);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[ci] cannot start the scheduler: {e}");
        return None;
    }
    Some(busy)
}

impl Scheduler {
    fn new(host: Arc<JobHost>, state_dir: PathBuf, config: CiConfig, busy: Arc<Busy>) -> Scheduler {
        let (tx, rx) = std::sync::mpsc::channel();
        let machines = config
            .machines
            .iter()
            .map(|(name, cfg)| (name.clone(), Machine::new(name, cfg.clone(), &state_dir)))
            .collect();
        let crons = config
            .repos
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    CronCache {
                        schedules: Vec::new(),
                        refreshed: None,
                    },
                )
            })
            .collect();
        Scheduler {
            busy,
            statuses: forge::poster(state_dir.clone()),
            host,
            state_dir,
            config,
            running: Vec::new(),
            tx,
            rx,
            machines,
            crons,
            last_cron_check: Local::now(),
            last_tick: Instant::now(),
            freeze_warned: HashSet::new(),
        }
    }

    // ------------------------------------------------------------ recovery

    /// Picks up the pipelines a previous process left running.
    fn recover(&mut self) {
        let mut recovered = Vec::new();
        for mut pipeline in load_pipelines(&self.state_dir) {
            if pipeline.status != PipelineStatus::Running {
                continue;
            }
            let mut watches = Vec::new();
            for job in &mut pipeline.jobs {
                if !job.status.is_active() {
                    continue;
                }
                let Some(run_dir) = job.run_dir.clone() else {
                    job.status = JobStatus::Pending;
                    continue;
                };
                if !run_dir.join("log").exists() {
                    // Never spawned: start it again from the beginning.
                    job.status = JobStatus::Pending;
                    job.run_dir = None;
                    continue;
                }
                // Frozen jobs are thawed below; busy handling freezes them
                // again if the machine is still in use.
                job.status = JobStatus::Running;
                if let Some(machine) = job.def.machine.as_ref().and_then(|m| self.machines.get(m)) {
                    machine.adopt();
                }
                watches.push((job.def.name.clone(), run_dir));
            }
            for (job, run_dir) in watches {
                self.watch_recovered(&pipeline, &job, run_dir);
            }
            pipeline.save(&self.state_dir);
            eprintln!(
                "[ci] recovered {} #{} ({})",
                pipeline.repo, pipeline.number, pipeline.event.git_ref
            );
            recovered.push(pipeline);
        }
        self.running = recovered;
        thaw_everything(&self.machines);
    }

    /// A thread that waits for the exit record of a run the previous
    /// process started; the mux's adoption writes it when the run ends.
    fn watch_recovered(&self, pipeline: &Pipeline, job: &str, run_dir: PathBuf) {
        let tx = self.tx.clone();
        let repo = pipeline.repo.clone();
        let number = pipeline.number;
        let job = job.to_owned();
        let spawned = std::thread::Builder::new()
            .name("zeughaus-ci-watch".into())
            .spawn(move || {
                loop {
                    if let Some(exit) = read_exit_record(&run_dir) {
                        let _ = tx.send(Msg::Done {
                            repo,
                            number,
                            job,
                            exit,
                        });
                        return;
                    }
                    std::thread::sleep(RECOVERY_POLL);
                }
            });
        if let Err(e) = spawned {
            eprintln!("[ci] cannot watch a recovered run: {e}");
        }
    }

    // ------------------------------------------------------------ tick

    fn tick(&mut self) {
        let elapsed = self.last_tick.elapsed().as_secs();
        self.last_tick = Instant::now();
        event::drain_inbox(&self.state_dir);
        self.fire_crons();
        for (path, event) in event::queued(&self.state_dir) {
            let key = event::queue_key(&event);
            if self.running.iter().any(|p| p.key == key) {
                continue;
            }
            self.start_pipeline(&path, event, key);
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        self.advance();
        self.apply_busy();
        self.machine_timeouts(elapsed);
        self.finish_pipelines();
        for machine in self.machines.values() {
            machine.tick();
        }
    }

    fn post(&self, pipeline: &Pipeline, job: &str, state: State, description: String) {
        let (Some(sha), Some(repo)) = (&pipeline.sha, self.config.repos.get(&pipeline.repo)) else {
            return;
        };
        let _ = self.statuses.send(Status {
            repo_name: pipeline.repo.clone(),
            repo: repo.clone(),
            sha: sha.clone(),
            job: job.to_owned(),
            state,
            description,
        });
    }

    // ------------------------------------------------------------ mirrors and cron

    /// Creates the mirror if needed and fetches every branch and tag.
    fn fetch(&self, name: &str, repo: &RepoConfig) -> Result<PathBuf, String> {
        let mirror = mirror_dir(&self.state_dir, name);
        if !mirror.join("HEAD").exists() {
            std::fs::create_dir_all(&mirror)
                .map_err(|e| format!("cannot create {}: {e}", mirror.display()))?;
            git(&mirror, &["init", "--bare", "-q"], &[])?;
            git(&mirror, &["remote", "add", "origin", &repo.url], &[])?;
            git(
                &mirror,
                &["config", "uploadpack.allowAnySHA1InWant", "true"],
                &[],
            )?;
        }
        let mut env = Vec::new();
        if let Some(secret) = &repo.fetch_token {
            let token = read_secret(&self.state_dir, secret)?;
            let basic = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{token}", repo.fetch_user));
            env.push(("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()));
            env.push(("GIT_CONFIG_KEY_0".to_owned(), "http.extraHeader".to_owned()));
            env.push((
                "GIT_CONFIG_VALUE_0".to_owned(),
                format!("Authorization: Basic {basic}"),
            ));
        }
        git(
            &mirror,
            &[
                "fetch",
                "--prune",
                "-q",
                "origin",
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
            &env,
        )?;
        Ok(mirror)
    }

    /// Re-reads the cron schedules at the head of the default branch.
    fn refresh_crons(&mut self, name: &str, mirror: &Path) {
        let Some(repo) = self.config.repos.get(name) else {
            return;
        };
        let head = format!("refs/heads/{}", repo.default_branch);
        let schedules = rev_parse(mirror, &format!("{head}^{{commit}}"))
            .and_then(|sha| read_ci(mirror, &sha))
            .and_then(|(files, containerfiles)| pipeline::load_jobs(&files, &containerfiles))
            .map(|jobs| pipeline::cron_schedules(&jobs));
        let cache = self.crons.entry(name.to_owned()).or_insert(CronCache {
            schedules: Vec::new(),
            refreshed: None,
        });
        cache.refreshed = Some(Instant::now());
        match schedules {
            Ok(schedules) => cache.schedules = schedules,
            Err(e) => eprintln!("[ci] {name}: cron schedules not read: {e}"),
        }
    }

    fn fire_crons(&mut self) {
        let stale: Vec<String> = self
            .crons
            .iter()
            .filter(|(_, c)| c.refreshed.is_none_or(|at| at.elapsed() >= CRON_REFRESH))
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            let Some(repo) = self.config.repos.get(&name).cloned() else {
                continue;
            };
            match self.fetch(&name, &repo) {
                Ok(mirror) => self.refresh_crons(&name, &mirror),
                Err(e) => {
                    eprintln!("[ci] {name}: fetch: {e}");
                    if let Some(cache) = self.crons.get_mut(&name) {
                        cache.refreshed = Some(Instant::now());
                    }
                }
            }
        }
        let now = Local::now();
        for (name, cache) in &self.crons {
            let Some(repo) = self.config.repos.get(name) else {
                continue;
            };
            for expr in &cache.schedules {
                let Ok(cron) = croner::Cron::from_str(expr) else {
                    continue;
                };
                let due = cron
                    .find_next_occurrence(&self.last_cron_check, false)
                    .is_ok_and(|next| next <= now);
                if !due {
                    continue;
                }
                let event = CiEvent {
                    repo: name.clone(),
                    kind: EventKind::Cron,
                    git_ref: repo.default_branch.clone(),
                    sha: None,
                    cron: Some(expr.clone()),
                    actor: "cron".to_owned(),
                    delivery: None,
                    received: super::now_secs(),
                };
                match event::write_inbox(&self.state_dir, &event) {
                    Ok(_) => eprintln!("[ci] {name}: cron {expr} due"),
                    Err(e) => eprintln!("[ci] {name}: cron {expr}: {e}"),
                }
            }
        }
        self.last_cron_check = now;
    }

    // ------------------------------------------------------------ pipelines

    fn start_pipeline(&mut self, queue_file: &Path, event: CiEvent, key: String) {
        let _ = std::fs::remove_file(queue_file);
        let name = event.repo.clone();
        let Some(repo) = self.config.repos.get(&name).cloned() else {
            eprintln!("[ci] event for unknown repo {name} dropped");
            return;
        };
        let mut pipeline = Pipeline {
            repo: name.clone(),
            number: 0,
            key,
            event: event.clone(),
            sha: None,
            status: PipelineStatus::Running,
            note: String::new(),
            created: super::now_secs(),
            finished: None,
            jobs: Vec::new(),
        };
        let setup = self.set_up(&name, &repo, &event, &mut pipeline);
        let jobs = match setup {
            Ok(jobs) => jobs,
            Err(e) => {
                eprintln!("[ci] {name} {} {}: {e}", event.kind, event.git_ref);
                pipeline.number = next_number(&self.state_dir, &name);
                pipeline.status = PipelineStatus::Error;
                pipeline.note = e.clone();
                pipeline.finished = Some(super::now_secs());
                pipeline.save(&self.state_dir);
                self.post(
                    &pipeline,
                    "ci",
                    State::Error,
                    format!("{e} (#{})", pipeline.number),
                );
                return;
            }
        };
        if jobs.is_empty() {
            eprintln!("[ci] {name} {} {}: no jobs", event.kind, event.git_ref);
            return;
        }
        let Some(sha) = pipeline.sha.clone() else {
            return;
        };
        pipeline.number = next_number(&self.state_dir, &name);
        let mirror = mirror_dir(&self.state_dir, &name);
        if let Err(e) = git(
            &mirror,
            &["update-ref", &format!("refs/ci/{}", pipeline.number), &sha],
            &[],
        ) {
            eprintln!("[ci] {name}: {e}");
            return;
        }
        pipeline.jobs = jobs
            .into_iter()
            .map(|def| JobRecord {
                def,
                status: JobStatus::Pending,
                note: String::new(),
                run_dir: None,
                code: None,
                started: None,
                finished: None,
                image_tag: None,
                active_secs: 0,
            })
            .collect();
        pipeline.save(&self.state_dir);
        eprintln!(
            "[ci] {name} #{} {} {} {}: {}",
            pipeline.number,
            event.kind,
            event.git_ref,
            &sha[..sha.len().min(7)],
            pipeline
                .jobs
                .iter()
                .map(|j| j.def.name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for job in &pipeline.jobs {
            self.post(
                &pipeline,
                &job.def.name,
                State::Pending,
                format!("queued (#{})", pipeline.number),
            );
        }
        self.running.push(pipeline);
    }

    /// Fetch, resolve and select: the jobs of the pipeline for `event`.
    /// Sets the pipeline's sha as soon as it is known, so an error after
    /// that can still be posted.
    fn set_up(
        &mut self,
        name: &str,
        repo: &RepoConfig,
        event: &CiEvent,
        pipeline: &mut Pipeline,
    ) -> Result<Vec<JobDef>, String> {
        let mirror = self.fetch(name, repo)?;
        self.refresh_crons(name, &mirror);
        let sha = match &event.sha {
            Some(sha) => {
                git(
                    &mirror,
                    &["cat-file", "-e", &format!("{sha}^{{commit}}")],
                    &[],
                )
                .map_err(|_| format!("commit {sha} is not in the mirror"))?;
                sha.clone()
            }
            None => {
                let refname = match event.kind {
                    EventKind::Tag => format!("refs/tags/{}^{{commit}}", event.git_ref),
                    EventKind::Push | EventKind::Cron => {
                        format!("refs/heads/{}^{{commit}}", event.git_ref)
                    }
                };
                rev_parse(&mirror, &refname)?
            }
        };
        pipeline.sha = Some(sha.clone());
        let (files, containerfiles) = read_ci(&mirror, &sha)?;
        let jobs = pipeline::load_jobs(&files, &containerfiles)?;
        let selected = pipeline::select(&jobs, event);
        Ok(jobs
            .into_iter()
            .filter(|job| selected.contains(&job.name))
            .collect())
    }

    fn find(&mut self, repo: &str, number: u64) -> Option<usize> {
        self.running
            .iter()
            .position(|p| p.repo == repo && p.number == number)
    }

    fn finish_pipelines(&mut self) {
        let mut i = 0;
        while i < self.running.len() {
            if !self.running[i].jobs.iter().all(|j| j.status.is_terminal()) {
                i += 1;
                continue;
            }
            let mut pipeline = self.running.remove(i);
            pipeline.status = if pipeline
                .jobs
                .iter()
                .all(|j| j.status == JobStatus::Succeeded)
            {
                PipelineStatus::Succeeded
            } else {
                PipelineStatus::Failed
            };
            pipeline.finished = Some(super::now_secs());
            pipeline.save(&self.state_dir);
            eprintln!(
                "[ci] {} #{} {}",
                pipeline.repo,
                pipeline.number,
                pipeline.status.as_str()
            );
            let mirror = mirror_dir(&self.state_dir, &pipeline.repo);
            if let Err(e) = git(
                &mirror,
                &["update-ref", "-d", &format!("refs/ci/{}", pipeline.number)],
                &[],
            ) {
                eprintln!("[ci] {}: {e}", pipeline.repo);
            }
            self.housekeeping(&pipeline);
        }
    }

    fn housekeeping(&self, finished: &Pipeline) {
        let in_use: HashSet<PathBuf> = self
            .running
            .iter()
            .flat_map(|p| {
                p.jobs
                    .iter()
                    .filter(|j| j.status.is_active())
                    .map(|j| workspace_dir(&self.state_dir, &p.repo, &j.def.name))
            })
            .collect();
        cleanup::enforce_budget(&self.state_dir, self.config.budget_gb, &in_use);
        let used_tags: HashSet<String> = self
            .running
            .iter()
            .chain(std::iter::once(finished))
            .filter(|p| p.repo == finished.repo)
            .flat_map(|p| p.jobs.iter().filter_map(|j| j.image_tag.clone()))
            .collect();
        if finished.jobs.iter().any(|j| j.def.image.is_some()) {
            cleanup::prune_images(&finished.repo, &used_tags);
        }
        let running: HashSet<u64> = self
            .running
            .iter()
            .filter(|p| p.repo == finished.repo)
            .map(|p| p.number)
            .collect();
        cleanup::prune_pipelines(&self.state_dir, &finished.repo, &running);
    }

    // ------------------------------------------------------------ jobs

    /// Whether a job of `repo` named `job` is active in any pipeline: one
    /// workspace, one job at a time.
    fn workspace_busy(&self, repo: &str, job: &str) -> bool {
        self.running.iter().any(|p| {
            p.repo == repo
                && p.jobs
                    .iter()
                    .any(|j| j.def.name == job && j.status.is_active())
        })
    }

    fn machine_busy(&self, machine: &str) -> bool {
        self.running.iter().any(|p| {
            p.jobs
                .iter()
                .any(|j| j.def.machine.as_deref() == Some(machine) && j.status.is_active())
        })
    }

    fn advance(&mut self) {
        let busy = self.busy.is_busy();
        for pi in 0..self.running.len() {
            for ji in 0..self.running[pi].jobs.len() {
                let status = self.running[pi].jobs[ji].status;
                if !matches!(status, JobStatus::Pending | JobStatus::Waiting) {
                    continue;
                }
                let pipeline = &self.running[pi];
                let job = &pipeline.jobs[ji];
                let need_status = |need: &str| {
                    pipeline
                        .jobs
                        .iter()
                        .find(|j| j.def.name == need)
                        .map(|j| j.status)
                };
                if let Some(failed) = job.def.needs.iter().find(|need| {
                    matches!(
                        need_status(need),
                        Some(JobStatus::Failed | JobStatus::Skipped)
                    )
                }) {
                    let note = format!("skipped: {failed} failed (#{})", pipeline.number);
                    let name = job.def.name.clone();
                    let pipeline = &mut self.running[pi];
                    pipeline.jobs[ji].status = JobStatus::Skipped;
                    pipeline.jobs[ji].note = note.clone();
                    pipeline.jobs[ji].finished = Some(super::now_secs());
                    pipeline.save(&self.state_dir);
                    let pipeline = self.running[pi].clone();
                    self.post(&pipeline, &name, State::Error, note);
                    continue;
                }
                if !job
                    .def
                    .needs
                    .iter()
                    .all(|need| need_status(need) == Some(JobStatus::Succeeded))
                {
                    continue;
                }
                if self.host.held() {
                    continue;
                }
                if busy && job.def.when_busy != WhenBusy::Run {
                    if status != JobStatus::Waiting {
                        let name = job.def.name.clone();
                        let number = pipeline.number;
                        let pipeline = &mut self.running[pi];
                        pipeline.jobs[ji].status = JobStatus::Waiting;
                        pipeline.jobs[ji].note = "machine busy".to_owned();
                        pipeline.save(&self.state_dir);
                        let pipeline = self.running[pi].clone();
                        self.post(
                            &pipeline,
                            &name,
                            State::Pending,
                            format!("waiting: machine busy (#{number})"),
                        );
                    }
                    continue;
                }
                let repo = pipeline.repo.clone();
                let name = job.def.name.clone();
                if self.workspace_busy(&repo, &name) {
                    continue;
                }
                if let Some(machine) = job.def.machine.clone()
                    && self.machine_busy(&machine)
                {
                    continue;
                }
                self.start_job(pi, ji);
            }
        }
    }

    /// Marks a job failed before it ran, and says why.
    fn fail_start(&mut self, pi: usize, ji: usize, note: String, state: State) {
        let pipeline = &mut self.running[pi];
        let job = &mut pipeline.jobs[ji];
        eprintln!(
            "[ci] {} #{} {}: {note}",
            pipeline.repo, pipeline.number, job.def.name
        );
        job.status = JobStatus::Failed;
        job.note = note.clone();
        job.finished = Some(super::now_secs());
        let name = job.def.name.clone();
        let number = pipeline.number;
        pipeline.save(&self.state_dir);
        let pipeline = self.running[pi].clone();
        self.post(&pipeline, &name, state, format!("{note} (#{number})"));
    }

    fn start_job(&mut self, pi: usize, ji: usize) {
        let run_dir = match self.host.new_run_dir() {
            Ok(dir) => dir,
            Err(e) => {
                // The state directory is unwritable; every job would fail
                // the same way, so this one waits for the next tick.
                eprintln!("[ci] {e}");
                return;
            }
        };
        let started = super::now_secs();
        {
            let job = &mut self.running[pi].jobs[ji];
            job.status = JobStatus::Starting;
            job.run_dir = Some(run_dir.clone());
            job.started = Some(started);
            job.note.clear();
            job.active_secs = 0;
        }
        self.running[pi].save(&self.state_dir);
        match self.prepare_job(pi, ji, &run_dir, started) {
            Ok(Prepared::Spawned) => {
                let pipeline = &mut self.running[pi];
                pipeline.jobs[ji].status = JobStatus::Running;
                pipeline.save(&self.state_dir);
                let pipeline = self.running[pi].clone();
                let job = &pipeline.jobs[ji];
                self.post(
                    &pipeline,
                    &job.def.name,
                    State::Pending,
                    format!("running on {} (#{})", place_name(&job.def), pipeline.number),
                );
            }
            Ok(Prepared::Booting) => {
                let pipeline = self.running[pi].clone();
                let job = &pipeline.jobs[ji];
                self.post(
                    &pipeline,
                    &job.def.name,
                    State::Pending,
                    format!(
                        "starting on {} (#{})",
                        place_name(&job.def),
                        pipeline.number
                    ),
                );
            }
            Err(e) => self.fail_start(pi, ji, e, State::Error),
        }
    }

    /// Stages a job's files and starts its run (or, for a machine job, the
    /// thread that boots the machine and then starts it).
    fn prepare_job(
        &mut self,
        pi: usize,
        ji: usize,
        run_dir: &Path,
        started: u64,
    ) -> Result<Prepared, String> {
        let pipeline = self.running[pi].clone();
        let job = &pipeline.jobs[ji].def;
        let repo = self
            .config
            .repos
            .get(&pipeline.repo)
            .cloned()
            .ok_or_else(|| format!("repo {} is no longer configured", pipeline.repo))?;
        let sha = pipeline.sha.clone().unwrap_or_default();
        let run_id = run_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mirror = mirror_dir(&self.state_dir, &pipeline.repo);

        // Inputs: the artifacts of every need, linked so the pipeline's
        // copy and the run's share the blocks.
        let inputs = run_dir.join("inputs");
        std::fs::create_dir_all(&inputs).map_err(|e| format!("cannot create inputs: {e}"))?;
        for need in &job.needs {
            let from = artifacts_dir(&self.state_dir, &pipeline.repo, pipeline.number).join(need);
            if from.exists() {
                link_tree(&from, &inputs.join(need))
                    .map_err(|e| format!("cannot stage inputs of {need}: {e}"))?;
            }
        }

        // Secrets: only those granted for this event.
        let mut secrets = Vec::new();
        for name in &job.secrets {
            if !repo.grants(name, &pipeline.event) {
                return Err(format!(
                    "secret {name} is not granted for {} {}",
                    pipeline.event.kind, pipeline.event.git_ref
                ));
            }
            secrets.push((name.clone(), read_secret(&self.state_dir, name)?));
        }
        let secret_dir = secret_dir(&self.state_dir, &run_id)?;

        let (cpus, workspace_in, output_in, inputs_in) = if job.image.is_some() {
            (
                self.config.containers.cpus,
                "/work".to_owned(),
                "/ci/run/artifacts".to_owned(),
                "/ci/inputs".to_owned(),
            )
        } else if let Some(machine) = &job.machine {
            let cfg = self
                .machines
                .get(machine)
                .map(|m| m.config.cpus)
                .ok_or_else(|| format!("machine {machine} is not configured"))?;
            (
                cfg,
                launch::guest_workspace(&pipeline.repo, &job.name),
                format!(r"{}\out", launch::guest_run_dir(&run_id)),
                format!(r"{}\in", launch::guest_run_dir(&run_id)),
            )
        } else {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1);
            (
                cores,
                workspace_dir(&self.state_dir, &pipeline.repo, &job.name)
                    .to_string_lossy()
                    .into_owned(),
                run_dir.join("artifacts").to_string_lossy().into_owned(),
                inputs.to_string_lossy().into_owned(),
            )
        };
        let mut env: Vec<(String, String)> = vec![
            ("CI".into(), "true".into()),
            ("ZEUGHAUS_CI".into(), "1".into()),
            ("CI_REPO".into(), pipeline.repo.clone()),
            ("CI_REPO_SLUG".into(), repo.slug.clone()),
            ("CI_EVENT".into(), pipeline.event.kind.to_string()),
            ("CI_REF".into(), pipeline.event.git_ref.clone()),
            ("CI_SHA".into(), sha.clone()),
            ("CI_PIPELINE".into(), pipeline.number.to_string()),
            ("CI_JOB".into(), job.name.clone()),
            ("CI_ACTOR".into(), pipeline.event.actor.clone()),
            ("CI_WORKSPACE".into(), workspace_in),
            ("CI_OUTPUT".into(), output_in),
            ("CI_INPUTS".into(), inputs_in),
            ("CI_CPUS".into(), cpus.to_string()),
            ("CARGO_BUILD_JOBS".into(), cpus.to_string()),
        ];
        if let Some(cron) = &pipeline.event.cron {
            env.push(("CI_CRON".into(), cron.clone()));
        }
        env.extend(job.env.iter().map(|(k, v)| (k.clone(), v.clone())));

        let workspace = workspace_dir(&self.state_dir, &pipeline.repo, &job.name);
        let ssh_host = job
            .machine
            .as_ref()
            .and_then(|m| self.machines.get(m))
            .map(|m| m.config.ssh_host.clone())
            .unwrap_or_default();
        let write = |path: &Path, text: &str| super::write_atomic(path, text.as_bytes());
        let mut tag = None;
        let place = if let Some(image) = &job.image {
            let containerfile = git(
                &mirror,
                &["show", &format!("{sha}:{CI_DIR}/{image}.Containerfile")],
                &[],
            )?;
            let digest = hex::encode(Sha256::digest(containerfile.as_bytes()));
            tag = Some(format!(
                "localhost/zci-{}-{image}:{}",
                pipeline.repo,
                &digest[..16]
            ));
            let mut file_env = env.clone();
            file_env.push(("ZEUGHAUS_RUN_DIR".into(), "/ci/run".into()));
            file_env.extend(secrets.iter().cloned());
            write_secret(&secret_dir.join("env"), &launch::env_file(&file_env))?;
            Place::Container {
                workspace: &workspace,
                tag: tag.as_deref().unwrap_or_default(),
                cpus: self.config.containers.cpus,
                memory: &self.config.containers.memory,
            }
        } else if job.machine.is_some() {
            let mut guest_env = env.clone();
            guest_env.extend(secrets.iter().cloned());
            write_secret(&secret_dir.join("env.ps1"), &launch::env_ps1(&guest_env))?;
            write(
                &run_dir.join("vm").join("inner.ps1"),
                &launch::inner_ps1(job, &run_id, pipeline.number),
            )?;
            write(
                &run_dir.join("vm").join("debug.ps1"),
                &launch::debug_ps1(&run_id),
            )?;
            let bundle = run_dir.join("src.bundle");
            git(
                &mirror,
                &[
                    "bundle",
                    "create",
                    "-q",
                    &bundle.to_string_lossy(),
                    &format!("refs/ci/{}", pipeline.number),
                ],
                &[],
            )?;
            Place::Machine {
                ssh_host: &ssh_host,
            }
        } else {
            write_secret(&secret_dir.join("env.sh"), &launch::env_sh(&secrets))?;
            Place::Host {
                workspace: &workspace,
            }
        };
        let is_host = matches!(place, Place::Host { .. });
        let launch = Launch {
            job,
            run_dir,
            run_id: &run_id,
            secret_dir: &secret_dir,
            mirror: &mirror,
            pipeline: pipeline.number,
            place,
        };
        write(&run_dir.join("launch.sh"), &launch::launch_sh(&launch))?;
        if job.machine.is_none() {
            write(&run_dir.join("inner.sh"), &launch::inner_sh(job))?;
        }
        self.running[pi].jobs[ji].image_tag = tag;

        let spec = JobSpec {
            label: format!("{}/{} #{}", pipeline.repo, job.name, pipeline.number),
            program: "/bin/sh".to_owned(),
            args: vec![run_dir.join("launch.sh").to_string_lossy().into_owned()],
            env: if is_host { env } else { Vec::new() },
            cwd: Some(run_dir.to_path_buf()),
            run_dir: run_dir.to_path_buf(),
            keep_on_failure: false,
            started,
            artifacts: Vec::new(),
            external: true,
        };

        let tx = self.tx.clone();
        let host = Arc::clone(&self.host);
        let repo_name = pipeline.repo.clone();
        let number = pipeline.number;
        let job_name = job.name.clone();
        let run_dir = run_dir.to_path_buf();
        if let Some(machine) = &job.machine {
            let machine = self
                .machines
                .get(machine)
                .cloned()
                .ok_or_else(|| format!("machine {machine} is not configured"))?;
            std::thread::Builder::new()
                .name("zeughaus-ci-job".into())
                .spawn(move || {
                    if let Err(note) = machine.acquire() {
                        let _ = tx.send(Msg::StartFailed {
                            repo: repo_name,
                            number,
                            job: job_name,
                            note,
                        });
                        return;
                    }
                    let handle = match host.spawn(spec) {
                        Ok(handle) => handle,
                        Err(note) => {
                            let _ = tx.send(Msg::StartFailed {
                                repo: repo_name,
                                number,
                                job: job_name,
                                note,
                            });
                            return;
                        }
                    };
                    let _ = tx.send(Msg::Spawned {
                        repo: repo_name.clone(),
                        number,
                        job: job_name.clone(),
                    });
                    wait_and_report(handle, &run_dir, started, &tx, repo_name, number, job_name);
                })
                .map_err(|e| format!("cannot start the job thread: {e}"))?;
            return Ok(Prepared::Booting);
        }
        let handle = self.host.spawn(spec)?;
        std::thread::Builder::new()
            .name("zeughaus-ci-job".into())
            .spawn(move || {
                wait_and_report(handle, &run_dir, started, &tx, repo_name, number, job_name);
            })
            .map_err(|e| format!("cannot start the job thread: {e}"))?;
        Ok(Prepared::Spawned)
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Spawned { repo, number, job } => {
                let Some(pi) = self.find(&repo, number) else {
                    return;
                };
                let Some(ji) = self.running[pi].jobs.iter().position(|j| j.def.name == job) else {
                    return;
                };
                self.running[pi].jobs[ji].status = JobStatus::Running;
                self.running[pi].save(&self.state_dir);
                let pipeline = self.running[pi].clone();
                self.post(
                    &pipeline,
                    &job,
                    State::Pending,
                    format!(
                        "running on {} (#{number})",
                        place_name(&pipeline.jobs[ji].def)
                    ),
                );
            }
            Msg::StartFailed {
                repo,
                number,
                job,
                note,
            } => {
                let Some(pi) = self.find(&repo, number) else {
                    return;
                };
                let Some(ji) = self.running[pi].jobs.iter().position(|j| j.def.name == job) else {
                    return;
                };
                self.fail_start(pi, ji, note, State::Error);
            }
            Msg::Done {
                repo,
                number,
                job,
                exit,
            } => self.finish_job(&repo, number, &job, exit),
        }
    }

    fn finish_job(&mut self, repo: &str, number: u64, job: &str, exit: RunExit) {
        let Some(pi) = self.find(repo, number) else {
            return;
        };
        let Some(ji) = self.running[pi].jobs.iter().position(|j| j.def.name == job) else {
            return;
        };
        let now = super::now_secs();
        let record = &self.running[pi].jobs[ji];
        let elapsed = now.saturating_sub(record.started.unwrap_or(now));
        let run_dir = record.run_dir.clone();
        let machine = record
            .def
            .machine
            .as_ref()
            .and_then(|m| self.machines.get(m))
            .cloned();
        let succeeded = exit.code == Some(0);
        let (state, note) = if succeeded {
            if let Some(run_dir) = &run_dir {
                let from = run_dir.join("artifacts");
                if from.exists() {
                    let to = artifacts_dir(&self.state_dir, repo, number).join(job);
                    if let Err(e) = link_tree(&from, &to) {
                        eprintln!("[ci] {repo} #{number} {job}: artifacts: {e}");
                    }
                }
            }
            (
                State::Success,
                format!("passed in {}m{}s", elapsed / 60, elapsed % 60),
            )
        } else {
            (State::Failure, failure_note(exit))
        };
        if let Some(machine) = &machine {
            machine.release();
            if !succeeded && let Some(run_dir) = &run_dir {
                machine.add_debug_run(run_dir.clone());
            }
        }
        let pipeline = &mut self.running[pi];
        let record = &mut pipeline.jobs[ji];
        record.status = if succeeded {
            JobStatus::Succeeded
        } else {
            JobStatus::Failed
        };
        record.code = exit.code;
        record.note = note.clone();
        record.finished = Some(now);
        eprintln!("[ci] {repo} #{number} {job}: {note}");
        pipeline.save(&self.state_dir);
        let pipeline = self.running[pi].clone();
        self.post(&pipeline, job, state, format!("{note} (#{number})"));
    }

    // ------------------------------------------------------------ busy, freeze, timeouts

    fn apply_busy(&mut self) {
        let busy = self.busy.is_busy();
        for pi in 0..self.running.len() {
            for ji in 0..self.running[pi].jobs.len() {
                let job = &self.running[pi].jobs[ji];
                if job.def.when_busy != WhenBusy::Freeze {
                    continue;
                }
                let target = match (busy, job.status) {
                    (true, JobStatus::Running) => JobStatus::Frozen,
                    (false, JobStatus::Frozen) => JobStatus::Running,
                    _ => continue,
                };
                let result = self.freeze(
                    &job.def,
                    job.run_dir.as_deref(),
                    target == JobStatus::Frozen,
                );
                let pipeline = &self.running[pi];
                let key = (pipeline.repo.clone(), pipeline.number, job.def.name.clone());
                if let Err(e) = result {
                    if self.freeze_warned.insert(key) {
                        eprintln!(
                            "[ci] {} #{} {}: cannot {}: {e}",
                            pipeline.repo,
                            pipeline.number,
                            job.def.name,
                            if target == JobStatus::Frozen {
                                "freeze"
                            } else {
                                "thaw"
                            }
                        );
                    }
                    continue;
                }
                self.freeze_warned.remove(&key);
                let name = job.def.name.clone();
                let description = if target == JobStatus::Frozen {
                    format!("paused: machine busy (#{})", pipeline.number)
                } else {
                    format!("running on {} (#{})", place_name(&job.def), pipeline.number)
                };
                let pipeline = &mut self.running[pi];
                pipeline.jobs[ji].status = target;
                pipeline.jobs[ji].note = if target == JobStatus::Frozen {
                    "paused: machine busy".to_owned()
                } else {
                    String::new()
                };
                pipeline.save(&self.state_dir);
                let pipeline = self.running[pi].clone();
                self.post(&pipeline, &name, State::Pending, description);
            }
        }
    }

    fn freeze(&self, job: &JobDef, run_dir: Option<&Path>, freeze: bool) -> Result<(), String> {
        if let Some(machine) = &job.machine {
            let machine = self
                .machines
                .get(machine)
                .ok_or_else(|| format!("machine {machine} is not configured"))?;
            return machine.monitor(if freeze { "stop" } else { "cont" });
        }
        let run_id = run_dir
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("the job has no run")?;
        podman(&[
            if freeze { "pause" } else { "unpause" },
            &format!("zci-{run_id}"),
        ])
    }

    /// The timeout of a machine job, whose clock runs only while it is not
    /// frozen. A host or container job times itself out inside `inner.sh`.
    fn machine_timeouts(&mut self, elapsed: u64) {
        let mut changed = Vec::new();
        for (pi, pipeline) in self.running.iter_mut().enumerate() {
            for job in &mut pipeline.jobs {
                if job.def.machine.is_none() || job.status != JobStatus::Running {
                    continue;
                }
                job.active_secs += elapsed;
                let limit = u64::from(job.def.timeout_minutes) * 60;
                let Some(run_dir) = &job.run_dir else {
                    continue;
                };
                if job.active_secs < limit || run_dir.join("timeout").exists() {
                    continue;
                }
                let _ = std::fs::write(run_dir.join("timeout"), b"");
                if let Some(pid) = std::fs::read_to_string(run_dir.join("ssh.pid"))
                    .ok()
                    .and_then(|t| t.trim().parse::<libc::pid_t>().ok())
                    .filter(|pid| *pid > 0)
                {
                    // SAFETY: a plain signal to the pid the launcher wrote.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
                eprintln!(
                    "[ci] {} #{} {}: timed out",
                    pipeline.repo, pipeline.number, job.def.name
                );
                changed.push(pi);
            }
        }
        for pi in changed {
            self.running[pi].save(&self.state_dir);
        }
    }
}

enum Prepared {
    Spawned,
    Booting,
}

fn wait_and_report(
    handle: Box<dyn zeughaus_job::RunHandle>,
    run_dir: &Path,
    started: u64,
    tx: &Sender<Msg>,
    repo: String,
    number: u64,
    job: String,
) {
    let exit = handle.wait();
    // The exit record is what pruning keeps or deletes a run by, and what a
    // restarted scheduler reads; the job node writes it for its runs, so
    // this does for the CI's.
    if let Err(e) = record_run_end(run_dir, exit, started, None, &[]) {
        eprintln!("[ci] {}: {e}", run_dir.display());
    }
    let _ = tx.send(Msg::Done {
        repo,
        number,
        job,
        exit,
    });
}

fn failure_note(exit: RunExit) -> String {
    match exit.code {
        Some(124) => "timed out".to_owned(),
        Some(70) => "failed: checkout (exit 70)".to_owned(),
        Some(71) => "failed: container image build (exit 71)".to_owned(),
        Some(74) => "failed: copying artifacts from the guest (exit 74)".to_owned(),
        Some(75) => "failed: guest unreachable (exit 75)".to_owned(),
        Some(code) => format!("failed with exit {code}"),
        None if exit.killed => "killed".to_owned(),
        None => "died from a signal".to_owned(),
    }
}

/// `host`, `image:<x>` or `machine:<x>`.
pub fn place_name(job: &JobDef) -> String {
    if let Some(image) = &job.image {
        format!("image:{image}")
    } else if let Some(machine) = &job.machine {
        format!("machine:{machine}")
    } else {
        "host".to_owned()
    }
}

/// Reads `code=` from a run's exit record.
fn read_exit_record(run_dir: &Path) -> Option<RunExit> {
    let text = std::fs::read_to_string(run_dir.join("exit")).ok()?;
    let mut exit = RunExit {
        code: None,
        killed: false,
    };
    let mut seen = false;
    for line in text.lines() {
        if let Some(code) = line.strip_prefix("code=") {
            exit.code = code.parse().ok();
            seen = true;
        } else if let Some(killed) = line.strip_prefix("killed=") {
            exit.killed = killed == "true";
        }
    }
    seen.then_some(exit)
}

/// Resumes every container and VM a previous process may have frozen.
fn thaw_everything(machines: &BTreeMap<String, Arc<Machine>>) {
    if let Ok(output) = Command::new("podman")
        .args([
            "ps",
            "--filter",
            "status=paused",
            "--filter",
            "name=^zci-",
            "--format",
            "{{.Names}}",
        ])
        .stderr(Stdio::null())
        .output()
    {
        for name in String::from_utf8_lossy(&output.stdout).lines() {
            if let Err(e) = podman(&["unpause", name]) {
                eprintln!("[ci] cannot unpause {name}: {e}");
            }
        }
    }
    for machine in machines.values() {
        if machine.config.dir.join("qemu.pid").exists() {
            let _ = machine.monitor("cont");
        }
    }
}

fn podman(args: &[&str]) -> Result<(), String> {
    let output = Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run podman: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// `git -C <dir> <args>`; stdout on success.
fn git(dir: &Path, args: &[&str], env: &[(String, String)]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn rev_parse(mirror: &Path, rev: &str) -> Result<String, String> {
    git(mirror, &["rev-parse", "--verify", "-q", rev], &[])
        .map(|out| out.trim().to_owned())
        .map_err(|_| format!("{rev} does not exist"))
}

/// The `.zeughaus-ci/` files of a commit: (name, text) of every job or helper file,
/// and the names of the Containerfiles.
type CiFiles = (Vec<(String, String)>, Vec<String>);

fn read_ci(mirror: &Path, sha: &str) -> Result<CiFiles, String> {
    let listing = git(mirror, &["ls-tree", sha, &format!("{CI_DIR}/")], &[])?;
    let mut files = Vec::new();
    let mut containerfiles = Vec::new();
    for line in listing.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        if meta.split_whitespace().nth(1) != Some("blob") {
            continue;
        }
        let Some(name) = path
            .strip_prefix(CI_DIR)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        if name.ends_with(".Containerfile") {
            containerfiles.push(name.to_owned());
            continue;
        }
        let text = git(mirror, &["show", &format!("{sha}:{path}")], &[])?;
        files.push((name.to_owned(), text));
    }
    Ok((files, containerfiles))
}

/// `$XDG_RUNTIME_DIR/zeughaus-ci/<run-id>`, or `<state>/ci/tmp/<run-id>`
/// without one: a tmpfs where it exists, mode 0700 either way.
fn secret_dir(state_dir: &Path, run_id: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|dir| PathBuf::from(dir).join("zeughaus-ci"))
        .unwrap_or_else(|| super::ci_dir(state_dir).join("tmp"));
    let dir = root.join(run_id);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Writes a file only its owner can read.
fn write_secret(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Hard-links every file under `from` into `to` (copying where a link is
/// refused), creating directories as needed.
fn link_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            link_tree(&source, &target)?;
        } else if kind.is_symlink() {
            let link = std::fs::read_link(&source)?;
            let _ = std::fs::remove_file(&target);
            std::os::unix::fs::symlink(link, &target)?;
        } else {
            let _ = std::fs::remove_file(&target);
            if std::fs::hard_link(&source, &target).is_err() {
                std::fs::copy(&source, &target)?;
            }
        }
    }
    Ok(())
}
