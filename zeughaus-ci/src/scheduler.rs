//! The scheduler thread: queued events become pipelines, pipelines become
//! assignments to the runners enlisted in this process.
//!
//! Every decision is persisted in `ci/pipelines/<repo>/<n>.json` before it
//! is acted on, so a restart finds its pipelines on disk. The runs
//! themselves live on the runners, which report them again when they
//! reconnect; that report settles every job that was active.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use zeughaus_link::enlist::{
    Assignment, Checkout, FromRunner, Input, InputSource, Place, RunEnd, RunReport, ToRunner,
    TransferEvent, Upload, WhenBusy, parse_run_id, run_id,
};

use crate::channels::{self, Channels};
use crate::config::{CiConfig, RepoConfig, RunnerOs};
use crate::event::{self, CiEvent, EventKind};
use crate::forge::{self, State, Status};
use crate::header::JobDef;
use crate::runners::Runners;
use crate::secrets::Secrets;
use crate::streak::{self, Streak};
use crate::{CI_DIR, cleanup, excerpt, manual, pipeline};

const TICK: Duration = Duration::from_secs(1);
/// How often a repository's cron schedules are re-read without a push.
const CRON_REFRESH: Duration = Duration::from_secs(3600);
/// Seconds after a job's timeout that a run whose runner is gone is given up.
const LOST_GRACE_SECS: u64 = 900;
/// How long a job of the default branch stays red before every further red
/// pipeline alerts again, not only the one that turned it red.
const RED_REMINDER: Duration = Duration::from_secs(3600);
/// Characters of a cause line an alert carries.
const ALERT_CAUSE_CHARS: usize = 200;
/// A job's note, and the forge's description, when its pipeline was cancelled.
const CANCELLED: &str = "cancelled";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    /// Pending, held back: the runner is offline or busy, or the workspace or
    /// machine is in use.
    Waiting,
    /// Handed to a runner, which has not reported the run started.
    Starting,
    Running,
    /// Running, paused because the runner is busy.
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

    pub fn is_active(self) -> bool {
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
#[serde(rename_all = "kebab-case")]
pub enum PipelineStatus {
    Running,
    Succeeded,
    Failed,
    /// The pipeline could not be set up: fetch, resolve or `.zeughaus-ci` validation.
    Error,
    /// A push to the default branch whose commit has no `.zeughaus-ci/`.
    NoJobs,
    /// Stopped on request; its unfinished jobs were killed or skipped.
    Cancelled,
}

impl PipelineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PipelineStatus::Running => "running",
            PipelineStatus::Succeeded => "succeeded",
            PipelineStatus::Failed => "failed",
            PipelineStatus::Error => "error",
            PipelineStatus::NoJobs => "no-jobs",
            PipelineStatus::Cancelled => "cancelled",
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
    pub code: Option<i32>,
    #[serde(default)]
    pub started: Option<u64>,
    #[serde(default)]
    pub finished: Option<u64>,
    /// When the job was handed to its runner: the start of its staging.
    /// `started` is overwritten when the runner reports the program began.
    #[serde(default)]
    pub assigned: Option<u64>,
    /// The inputs that moved to this job's runner, one entry per need.
    #[serde(default)]
    pub transfers: Vec<TransferRecord>,
    /// The runner the job is placed on, decided when the pipeline is set up.
    #[serde(default)]
    pub runner: Option<String>,
    /// Where on that runner it runs.
    #[serde(default)]
    pub place: Option<Place>,
    /// The run left an artifact set, which its runner holds for the jobs
    /// that need it. Cleared once the last of them has ended and the runner
    /// was told to drop the set.
    #[serde(default)]
    pub artifacts: bool,
    /// The end of a failed run's log (see [`excerpt`]).
    #[serde(default)]
    pub excerpt: Option<String>,
}

/// One artifact set moving to the runner of a consuming job. Times are
/// seconds since the UNIX epoch, like [`JobRecord::started`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferRecord {
    pub need: String,
    pub started: u64,
    pub finished: Option<u64>,
    pub bytes: Option<u64>,
    pub error: Option<String>,
}

impl JobRecord {
    /// The line of a failed job's excerpt that says why it failed (see
    /// [`excerpt::cause`]); `None` for a job that did not fail. It is the
    /// job's output as it stands: a caller that shows it to people the job's
    /// secrets are not for goes through [`shareable_cause`].
    pub fn cause(&self) -> Option<String> {
        if self.status != JobStatus::Failed {
            return None;
        }
        self.excerpt.as_deref().and_then(excerpt::cause)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub repo: String,
    pub number: u64,
    pub key: String,
    pub event: CiEvent,
    pub sha: Option<String>,
    /// The first line of the commit's message, read from the mirror once
    /// the sha is known; empty in records from before it was kept.
    #[serde(default)]
    pub subject: String,
    pub status: PipelineStatus,
    #[serde(default)]
    pub note: String,
    pub created: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    pub jobs: Vec<JobRecord>,
    /// See [`channels`]; empty in records from before channels existed.
    #[serde(default)]
    pub channel: String,
    /// An editor cancelled the pipeline: its jobs are killed or skipped
    /// and it ends as [`PipelineStatus::Cancelled`].
    #[serde(default)]
    pub cancelled: bool,
    /// The pipeline's channel is marked `clean` (see [`channels`]): the first
    /// job to use a place's workspace starts it from nothing.
    #[serde(default)]
    pub clean: bool,
}

fn pipelines_dir(state_dir: &Path, repo: &str) -> PathBuf {
    crate::ci_dir(state_dir).join("pipelines").join(repo)
}

/// Where a job's transcript is kept: `wal` in this directory is the CI's
/// copy of the run's WAL, appended as the runner streams it, and `exit`
/// holds `code=<n|none>` once the run ended. Beside the pipeline's record,
/// so retention takes it with the record.
pub fn transcript_dir(state_dir: &Path, repo: &str, number: u64, job: &str) -> PathBuf {
    pipelines_dir(state_dir, repo)
        .join(format!("{number}.runs"))
        .join(job)
}

fn mirror_dir(state_dir: &Path, repo: &str) -> PathBuf {
    crate::ci_dir(state_dir)
        .join("mirrors")
        .join(format!("{repo}.git"))
}

impl Pipeline {
    fn path(&self, state_dir: &Path) -> PathBuf {
        pipelines_dir(state_dir, &self.repo).join(format!("{}.json", self.number))
    }

    /// Writes the record, then announces it on `changes` for the host loop
    /// to tell editors. A receiver that went away is not an error: the
    /// record is on disk either way.
    fn save(&self, state_dir: &Path, changes: &Sender<CiChange>) {
        if let Err(e) = crate::files::write_json(&self.path(state_dir), self) {
            eprintln!(
                "[ci] cannot save pipeline {} #{}: {e}",
                self.repo, self.number
            );
            return;
        }
        let _ = changes.send(CiChange {
            repo: self.repo.clone(),
            channel: self.channel(),
            number: self.number,
        });
    }

    /// The channel the pipeline belongs to; a record from before channels
    /// existed has none stored and is sorted by its event.
    pub fn channel(&self) -> String {
        if self.channel.is_empty() {
            channels::fallback(&self.event)
        } else {
            self.channel.clone()
        }
    }
}

/// Every pipeline record on disk, newest first.
pub fn load_pipelines(state_dir: &Path) -> Vec<Pipeline> {
    let root = crate::ci_dir(state_dir).join("pipelines");
    let mut out = Vec::new();
    let Ok(repos) = std::fs::read_dir(&root) else {
        return out;
    };
    for repo in repos.flatten() {
        out.extend(read_records(&repo.path()));
    }
    out.sort_by_key(|p| std::cmp::Reverse((p.created, p.number)));
    out
}

/// Fills `subject` of stored records that predate the field, from each
/// repository's mirror, one `git log` per repository and chunk of shas.
/// Idempotent: a record that has a subject or whose commit the mirror no
/// longer has is left alone, so a second run changes nothing. A manual run's
/// subject is made from its summary, and its commit is in no mirror.
fn backfill_subjects(state_dir: &Path, changes: &Sender<CiChange>) {
    let mut pipelines: Vec<Pipeline> = load_pipelines(state_dir)
        .into_iter()
        .filter(|p| p.subject.is_empty() && p.sha.is_some() && p.event.kind != EventKind::Manual)
        .collect();
    pipelines.sort_by(|a, b| a.repo.cmp(&b.repo));
    for group in pipelines.chunk_by_mut(|a, b| a.repo == b.repo) {
        let repo = group[0].repo.clone();
        let mirror = mirror_dir(state_dir, &repo);
        let mut shas: Vec<String> = group.iter().filter_map(|p| p.sha.clone()).collect();
        shas.sort();
        shas.dedup();
        let mut subjects = HashMap::new();
        let mut failed = false;
        for chunk in shas.chunks(100) {
            let mut args = vec!["log", "--no-walk", "--format=%H%x00%s"];
            args.extend(chunk.iter().map(String::as_str));
            match git(&mirror, &args, &[]) {
                Ok(out) => subjects.extend(parse_subjects(&out)),
                Err(_) => failed = true,
            }
        }
        if failed {
            eprintln!("[ci] cannot read commit subjects of {repo} from its mirror");
        }
        for pipeline in group.iter_mut() {
            let subject = pipeline.sha.as_ref().and_then(|s| subjects.get(s));
            if let Some(subject) = subject.filter(|s| !s.is_empty()) {
                pipeline.subject = subject.clone();
                pipeline.save(state_dir, changes);
            }
        }
    }
}

/// `sha NUL subject` lines of `git log --format=%H%x00%s`.
fn parse_subjects(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\0')?;
            Some((sha.to_owned(), subject.trim().to_owned()))
        })
        .collect()
}

/// The pipeline records of one repository, in no particular order.
pub fn load_repo_pipelines(state_dir: &Path, repo: &str) -> Vec<Pipeline> {
    read_records(&pipelines_dir(state_dir, repo))
}

fn read_records(dir: &Path) -> Vec<Pipeline> {
    let Ok(files) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    files
        .flatten()
        .map(|file| file.path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .filter_map(|path| crate::files::read_json::<Pipeline>(&path).ok())
        .collect()
}

/// The record of pipeline `number` of `repo`.
pub fn load_pipeline(state_dir: &Path, repo: &str, number: u64) -> Result<Pipeline, String> {
    let path = pipelines_dir(state_dir, repo).join(format!("{number}.json"));
    crate::files::read_json(&path).map_err(|e| format!("{repo} #{number}: {e}"))
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

/// What the `/enlist` service tells the scheduler. `Output` frames never
/// come this way: the service appends them to the transcript itself.
pub enum Msg {
    /// A runner connected and reports the runs it knows and the artifact
    /// sets it holds.
    RunnerHello {
        name: String,
        runs: Vec<RunReport>,
        artifacts: Vec<String>,
    },
    FromRunner {
        name: String,
        message: FromRunner,
    },
    /// The runner's connection ended.
    RunnerGone {
        name: String,
    },
}

/// A selected job and where it runs.
struct Planned {
    def: JobDef,
    runner: String,
    place: Place,
}

struct CronCache {
    schedules: Vec<String>,
    refreshed: Option<Instant>,
}

struct Scheduler {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    running: Vec<Pipeline>,
    rx: Receiver<Msg>,
    runners: Arc<Runners>,
    statuses: Sender<Status>,
    /// The only reader of OpenBao in this process; jobs never see its token.
    secrets: Arc<Secrets>,
    crons: BTreeMap<String, CronCache>,
    last_cron_check: DateTime<Local>,
    /// When a job's needs were first all met while it had no runner to go
    /// to: its wait for the runner is counted from then. Not persisted; a
    /// restart starts the wait over.
    ready_since: HashMap<(String, u64, String), u64>,
    alerts: Sender<Alert>,
    /// Every record written, for the host loop to announce to editors.
    changes: Sender<CiChange>,
}

/// A default branch that turned red, or stays red: for the editors'
/// desktops.
pub struct Alert {
    pub title: String,
    pub body: String,
}

/// A pipeline record that was just written. The main loop turns it into an
/// event, and the editor asks `/ci` for the rows it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiChange {
    pub repo: String,
    pub channel: String,
    pub number: u64,
}

/// What the scheduler shares with the main loop and the enlist service.
pub struct Started {
    /// Alerts the main loop publishes to editors.
    pub alerts: Receiver<Alert>,
    /// Pipeline records as the scheduler writes them, for the main loop to
    /// announce to editors.
    pub changes: Receiver<CiChange>,
    /// Where the enlist service reports what runners say.
    pub tx: Sender<Msg>,
}

/// Starts the CI scheduler thread.
pub fn start(
    state_dir: &Path,
    config: Arc<CiConfig>,
    secrets: Arc<Secrets>,
    runners: Arc<Runners>,
) -> Started {
    eprintln!(
        "[ci] {} repos, {} runners",
        config.repos.len(),
        config.runners.len()
    );
    let (alerts, alerts_rx) = std::sync::mpsc::channel();
    let (changes, changes_rx) = std::sync::mpsc::channel();
    let (tx, rx) = std::sync::mpsc::channel();
    let state_dir = state_dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("zeughaus-ci".into())
        .spawn(move || {
            let mut scheduler =
                Scheduler::new(state_dir, config, secrets, runners, rx, alerts, changes);
            scheduler.recover();
            loop {
                scheduler.tick();
                std::thread::sleep(TICK);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[ci] cannot start the scheduler: {e}");
        std::process::exit(1);
    }
    Started {
        alerts: alerts_rx,
        changes: changes_rx,
        tx,
    }
}

impl Scheduler {
    fn new(
        state_dir: PathBuf,
        config: Arc<CiConfig>,
        secrets: Arc<Secrets>,
        runners: Arc<Runners>,
        rx: Receiver<Msg>,
        alerts: Sender<Alert>,
        changes: Sender<CiChange>,
    ) -> Scheduler {
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
            statuses: forge::poster(state_dir.clone(), Arc::clone(&secrets)),
            secrets,
            state_dir,
            config,
            running: Vec::new(),
            rx,
            runners,
            crons,
            last_cron_check: Local::now(),
            ready_since: HashMap::new(),
            alerts,
            changes,
        }
    }

    // ------------------------------------------------------------ recovery

    /// Picks up the pipelines a previous process left running. Their active
    /// jobs keep their status and runner: each runner reports its runs when it
    /// connects, and that report settles the job.
    fn recover(&mut self) {
        backfill_subjects(&self.state_dir, &self.changes);
        manual::remove_stale(&self.state_dir, manual::STALE_AFTER);
        for pipeline in load_pipelines(&self.state_dir) {
            if pipeline.status != PipelineStatus::Running {
                continue;
            }
            for job in &pipeline.jobs {
                if let (true, Some(runner)) = (job.status.is_active(), &job.runner) {
                    let run = run_id(&pipeline.repo, pipeline.number, &job.def.name);
                    self.runners.assign(&run, runner);
                }
            }
            eprintln!(
                "[ci] recovered {} #{} ({})",
                pipeline.repo, pipeline.number, pipeline.event.git_ref
            );
            let cancelled = pipeline.cancelled;
            self.running.push(pipeline);
            if cancelled {
                let pi = self.running.len() - 1;
                self.apply_cancel(pi);
            }
        }
    }

    // ------------------------------------------------------------ tick

    fn tick(&mut self) {
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
        self.cancel_requests();
        self.advance();
        self.lost_runners();
        self.drop_artifacts();
        self.finish_pipelines();
    }

    fn post(&self, pipeline: &Pipeline, job: &str, state: State, description: String) {
        // The commit of a manual run is a snapshot no forge has.
        if pipeline.event.kind == EventKind::Manual {
            return;
        }
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
            let token = self.secrets.get(secret).map_err(|e| e.to_string())?;
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
        )
        .map_err(|e| match &repo.fetch_token {
            Some(secret)
                if ["could not read Username", " 401", " 403"]
                    .iter()
                    .any(|s| e.contains(s)) =>
            {
                format!(
                    "{e} (fetch token rejected: expired or revoked? check {secret} in {})",
                    self.secrets.document()
                )
            }
            _ => e,
        })?;
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
                    upload: None,
                    received: crate::now_secs(),
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
        // A manual run is set up from its upload and names a repository of
        // ci.toml, which may be gone by now: that is its error to record.
        let repo = self.config.repos.get(&name).cloned();
        if repo.is_none() && event.kind != EventKind::Manual {
            eprintln!("[ci] event for unknown repo {name} dropped");
            return;
        }
        let mut pipeline = Pipeline {
            repo: name.clone(),
            number: 0,
            key,
            event: event.clone(),
            sha: None,
            subject: String::new(),
            status: PipelineStatus::Running,
            cancelled: false,
            note: String::new(),
            created: crate::now_secs(),
            finished: None,
            jobs: Vec::new(),
            channel: channels::fallback(&event),
            clean: false,
        };
        let setup = match (event.kind, &repo) {
            (EventKind::Manual, _) => self.set_up_manual(&event, &mut pipeline),
            (_, Some(repo)) => self.set_up(&name, repo, &event, &mut pipeline),
            // Dropped above.
            (_, None) => return,
        };
        let jobs = match setup {
            Ok(Some(jobs)) => jobs,
            Ok(None) => {
                if let Some(repo) = &repo {
                    self.no_ci_folder(pipeline, repo);
                }
                return;
            }
            Err(e) => {
                eprintln!("[ci] {name} {} {}: {e}", event.kind, event.git_ref);
                pipeline.number = next_number(&self.state_dir, &name);
                pipeline.status = PipelineStatus::Error;
                pipeline.note = e.clone();
                pipeline.finished = Some(crate::now_secs());
                pipeline.save(&self.state_dir, &self.changes);
                self.post(
                    &pipeline,
                    "ci",
                    State::Error,
                    format!("{e} (#{})", pipeline.number),
                );
                // No job will read the upload of a manual run that failed to
                // start: its uploader hears the number, and the upload goes.
                self.report_manual(&pipeline);
                self.drop_manual(&pipeline);
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
        pipeline.jobs = jobs
            .into_iter()
            .map(|planned| JobRecord {
                def: planned.def,
                status: JobStatus::Pending,
                note: String::new(),
                code: None,
                started: None,
                assigned: None,
                transfers: Vec::new(),
                finished: None,
                runner: Some(planned.runner),
                place: Some(planned.place),
                artifacts: false,
                excerpt: None,
            })
            .collect();
        pipeline.save(&self.state_dir, &self.changes);
        self.report_manual(&pipeline);
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

    /// A commit without `.zeughaus-ci/`. Silent on other refs, where most
    /// branches have no jobs on purpose; on the default branch it means CI
    /// is off, which is recorded and posted as a `zeughaus/pipeline` error.
    fn no_ci_folder(&mut self, mut pipeline: Pipeline, repo: &RepoConfig) {
        let sha = pipeline.sha.clone().unwrap_or_default();
        let mut note = format!("no {CI_DIR}/ in {}", &sha[..sha.len().min(7)]);
        eprintln!(
            "[ci] {} {} {}: {note}",
            pipeline.repo, pipeline.event.kind, pipeline.event.git_ref
        );
        if !(matches!(pipeline.event.kind, EventKind::Push)
            && pipeline.event.git_ref == repo.default_branch)
        {
            return;
        }
        let mirror = mirror_dir(&self.state_dir, &pipeline.repo);
        if git(&mirror, &["ls-tree", &sha, ".ci"], &[]).is_ok_and(|out| !out.trim().is_empty()) {
            note.push_str("; found .ci/, which the runner no longer reads");
        }
        pipeline.number = next_number(&self.state_dir, &pipeline.repo);
        pipeline.status = PipelineStatus::NoJobs;
        pipeline.note = note.clone();
        pipeline.finished = Some(crate::now_secs());
        pipeline.save(&self.state_dir, &self.changes);
        self.post(
            &pipeline,
            "pipeline",
            State::Error,
            forge::describe(&note, pipeline.number),
        );
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
    ) -> Result<Option<Vec<Planned>>, String> {
        let mirror = self.fetch(name, repo)?;
        self.refresh_crons(name, &mirror);
        let sha = event_commit(&mirror, event)?;
        pipeline.sha = Some(sha.clone());
        // Only shown, so a commit git cannot describe leaves it empty.
        pipeline.subject = git(&mirror, &["log", "-1", "--format=%s", &sha], &[])
            .map(|subject| subject.trim().to_owned())
            .unwrap_or_default();
        let (files, containerfiles) = read_ci(&mirror, &sha)?;
        let channels = Channels::from_files(&files)?;
        pipeline.channel = channels.of(event);
        pipeline.clean = channels.is_clean(&pipeline.channel);
        // git has no empty directories: nothing listed is no folder.
        if files.is_empty() && containerfiles.is_empty() {
            return Ok(None);
        }
        let jobs = pipeline::load_jobs(&files, &containerfiles)?;
        let selected = pipeline::select(&jobs, event, &pipeline.channel);
        let mut planned = Vec::new();
        for def in jobs.into_iter().filter(|job| selected.contains(&job.name)) {
            let (runner, place) = place_job(&self.config, name, &def, |image| {
                containerfile_at(&mirror, &sha, image)
            })?;
            planned.push(Planned { def, runner, place });
        }
        Ok(Some(planned))
    }

    /// The pipeline of a manual run: the jobs its upload names, read from the
    /// `.zeughaus-ci/` of the snapshot (see [`manual`]) and run in the working
    /// trees the upload holds, each in the workspace of its place, where the
    /// repository's pipelines build too.
    ///
    /// A need on a job the upload does not name is dropped, and no job gets
    /// secrets: the code of a working tree is not what a repository granted
    /// them to. Manual runs run on the host of a runner or in a container on
    /// it; a machine is a Windows VM, whose workspace a run would copy from
    /// one bundle.
    fn set_up_manual(
        &self,
        event: &CiEvent,
        pipeline: &mut Pipeline,
    ) -> Result<Option<Vec<Planned>>, String> {
        let id = event
            .upload
            .as_deref()
            .filter(|id| manual::valid_upload_id(id))
            .ok_or("a manual event carries no upload")?;
        let upload = manual::read_upload(&self.state_dir, id)
            .map_err(|_| format!("manual upload {id} is gone"))?;
        let main = upload
            .dirs
            .first()
            .ok_or("a manual upload carries no checkout")?;
        pipeline.sha = Some(main.sha.clone());
        pipeline.clean = false;

        let repo = self
            .config
            .repos
            .get(&event.repo)
            .ok_or_else(|| format!("{} is no repository of ci.toml", event.repo))?;
        let mirror = manual::mirror_dir(&self.state_dir, &event.repo);
        fetch_manual_bundle(&mirror, &manual::bundle_path(&self.state_dir, id, 0))?;
        let (files, containerfiles) = read_ci(&mirror, &main.sha)?;
        if files.is_empty() && containerfiles.is_empty() {
            return Err(format!("no {CI_DIR}/ in the working tree"));
        }
        // The channel a push to the default branch lands in.
        pipeline.channel = Channels::from_files(&files)?.of(&CiEvent {
            kind: EventKind::Push,
            git_ref: repo.default_branch.clone(),
            ..event.clone()
        });
        pipeline.subject = format!(
            "{} ({})",
            manual::subject(&upload.summary),
            upload.jobs.join(", ")
        );

        let jobs = pipeline::load_jobs(&files, &containerfiles)?;
        if let Some(missing) = upload
            .jobs
            .iter()
            .find(|name| jobs.iter().all(|job| job.name != **name))
        {
            let names: Vec<&str> = jobs.iter().map(|job| job.name.as_str()).collect();
            return Err(format!(
                "no job {missing} in {CI_DIR}/ of the working tree; its jobs are {}",
                names.join(", ")
            ));
        }
        let mut planned = Vec::new();
        for mut def in jobs
            .into_iter()
            .filter(|job| upload.jobs.contains(&job.name))
        {
            def.needs.retain(|need| upload.jobs.contains(need));
            def.secrets.clear();
            let (runner, place) = place_job(&self.config, &event.repo, &def, |image| {
                containerfile_at(&mirror, &main.sha, image)
            })?;
            if let Place::Machine { name } = &place {
                return Err(format!(
                    "manual runs run on a runner's host or in a container; {} runs on the machine {name}",
                    def.name
                ));
            }
            planned.push(Planned { def, runner, place });
        }
        Ok(Some(planned))
    }

    /// Tells the `/manual` service which pipeline its upload became. The
    /// record is on disk by now, so the uploader can ask for it at once.
    fn report_manual(&self, pipeline: &Pipeline) {
        let Some(id) = manual::upload_of(pipeline) else {
            return;
        };
        if let Err(e) = manual::write_number(&self.state_dir, id, pipeline.number) {
            eprintln!("[ci] {} #{}: {e}", pipeline.repo, pipeline.number);
        }
    }

    /// A manual run is over, or never began: its upload goes, and so does the
    /// mirror its `.zeughaus-ci/` was read from unless another manual run of
    /// the repository still runs. `pipeline` is no longer among the running
    /// ones.
    fn drop_manual(&self, pipeline: &Pipeline) {
        let Some(id) = manual::upload_of(pipeline) else {
            return;
        };
        manual::remove_upload(&self.state_dir, id);
        let others = self
            .running
            .iter()
            .any(|p| p.repo == pipeline.repo && p.event.kind == EventKind::Manual);
        if others {
            return;
        }
        let mirror = manual::mirror_dir(&self.state_dir, &pipeline.repo);
        if let Err(e) = std::fs::remove_dir_all(&mirror)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("[ci] cannot remove {}: {e}", mirror.display());
        }
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
            } else if pipeline.cancelled {
                PipelineStatus::Cancelled
            } else {
                PipelineStatus::Failed
            };
            pipeline.finished = Some(crate::now_secs());
            pipeline.save(&self.state_dir, &self.changes);
            eprintln!(
                "[ci] {} #{} {}",
                pipeline.repo,
                pipeline.number,
                pipeline.status.as_str()
            );
            if pipeline.status == PipelineStatus::Failed && pipeline.event.kind != EventKind::Manual
            {
                self.alert_red(&pipeline);
            }
            self.ready_since.retain(|(repo, number, _), _| {
                !(*repo == pipeline.repo && *number == pipeline.number)
            });
            self.drop_manual(&pipeline);
            self.housekeeping(&pipeline);
        }
    }

    fn housekeeping(&self, finished: &Pipeline) {
        let running: HashSet<u64> = self
            .running
            .iter()
            .filter(|p| p.repo == finished.repo)
            .map(|p| p.number)
            .collect();
        cleanup::prune_pipelines(&self.state_dir, &finished.repo, &running);
    }

    // ------------------------------------------------------------ jobs

    /// What keeps job `ji` of pipeline `pi` out of its workspace on `runner`,
    /// `None` when it may take it. The workspace is the job's place (see
    /// [`Place::workspace`]) per repository and runner, shared by the jobs of
    /// every pipeline and manual run there. They take it one at a time, so
    /// that each finds the tree as the one before left it.
    ///
    /// An active job there holds it. Otherwise a job waits for every job
    /// there that has not ended and sorts before it by (pipeline number,
    /// position in its pipeline): the oldest goes first. Only older jobs
    /// block, and a job's needs are earlier in its pipeline, so no two jobs
    /// wait for each other.
    fn workspace_wait(&self, pi: usize, ji: usize, runner: &str) -> Option<String> {
        let pipeline = &self.running[pi];
        let ws = pipeline.jobs[ji].place.as_ref()?.workspace();
        let shares = |q: &Pipeline, k: &JobRecord| {
            q.repo == pipeline.repo
                && k.runner.as_deref() == Some(runner)
                && k.place.as_ref().is_some_and(|p| p.workspace() == ws)
        };
        let waiting_for = |name: &str| format!("waiting for {name} in workspace {ws}");

        for (qi, q) in self.running.iter().enumerate() {
            let active = q
                .jobs
                .iter()
                .enumerate()
                .find(|&(ki, k)| (qi, ki) != (pi, ji) && shares(q, k) && k.status.is_active());
            if let Some((_, k)) = active {
                return Some(if qi == pi {
                    waiting_for(&k.def.name)
                } else {
                    format!("workspace {ws} in use by #{}", q.number)
                });
            }
        }
        for (qi, q) in self.running.iter().enumerate() {
            let older = q.jobs.iter().enumerate().find(|&(ki, k)| {
                (q.number, ki) < (pipeline.number, ji) && shares(q, k) && !k.status.is_terminal()
            });
            if let Some((_, k)) = older {
                return Some(if qi == pi {
                    waiting_for(&k.def.name)
                } else {
                    format!("workspace {ws} queued for #{}", q.number)
                });
            }
        }
        None
    }

    fn machine_busy(&self, machine: &str) -> bool {
        self.running.iter().any(|p| {
            p.jobs.iter().any(|j| {
                j.status.is_active()
                    && matches!(&j.place, Some(Place::Machine { name }) if name == machine)
            })
        })
    }

    fn advance(&mut self) {
        for pi in 0..self.running.len() {
            for ji in 0..self.running[pi].jobs.len() {
                let status = self.running[pi].jobs[ji].status;
                if !matches!(status, JobStatus::Pending | JobStatus::Waiting) {
                    continue;
                }
                if self.running[pi].cancelled {
                    // A job put back to pending by a lost runner stays out.
                    self.cancel_job(pi, ji);
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
                if let Some((failed, failed_status)) = job.def.needs.iter().find_map(|need| {
                    need_status(need)
                        .filter(|s| matches!(s, JobStatus::Failed | JobStatus::Skipped))
                        .map(|s| (need, s))
                }) {
                    let how = if failed_status == JobStatus::Skipped {
                        "skipped"
                    } else {
                        "failed"
                    };
                    let note = format!("skipped: {failed} {how} (#{})", pipeline.number);
                    let name = job.def.name.clone();
                    let pipeline = &mut self.running[pi];
                    pipeline.jobs[ji].status = JobStatus::Skipped;
                    pipeline.jobs[ji].note = note.clone();
                    pipeline.jobs[ji].finished = Some(crate::now_secs());
                    pipeline.save(&self.state_dir, &self.changes);
                    self.drop_prefetch(pi, ji);
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
                self.assign(pi, ji);
            }
        }
    }

    /// The id of the run of job `ji` of pipeline `pi`.
    fn run_of(&self, pi: usize, ji: usize) -> String {
        let pipeline = &self.running[pi];
        run_id(&pipeline.repo, pipeline.number, &pipeline.jobs[ji].def.name)
    }

    /// The pipeline and job index a run id names, among the running pipelines.
    fn locate(&self, run: &str) -> Option<(usize, usize)> {
        let (repo, number, job) = parse_run_id(run)?;
        let pi = self
            .running
            .iter()
            .position(|p| p.repo == repo && p.number == number)?;
        let ji = self.running[pi]
            .jobs
            .iter()
            .position(|j| j.def.name == job)?;
        Some((pi, ji))
    }

    /// Like [`Scheduler::locate`], for a run that is active on `runner`.
    fn active_on(&self, runner: &str, run: &str) -> Option<(usize, usize)> {
        self.locate(run).filter(|&(pi, ji)| {
            let job = &self.running[pi].jobs[ji];
            job.status.is_active() && job.runner.as_deref() == Some(runner)
        })
    }

    /// Keeps a job waiting, and says for what; the forge hears it once per
    /// reason.
    fn set_waiting(&mut self, pi: usize, ji: usize, note: String) {
        let job = &mut self.running[pi].jobs[ji];
        if job.status == JobStatus::Waiting && job.note == note {
            return;
        }
        job.status = JobStatus::Waiting;
        job.note = note.clone();
        let name = job.def.name.clone();
        self.running[pi].save(&self.state_dir, &self.changes);
        let pipeline = self.running[pi].clone();
        self.post(
            &pipeline,
            &name,
            State::Pending,
            format!("waiting: {note} (#{})", pipeline.number),
        );
    }

    /// Marks a job failed, and says why.
    fn fail_job(&mut self, pi: usize, ji: usize, note: String, state: State) {
        if self.running[pi].cancelled {
            self.cancel_job(pi, ji);
            return;
        }
        let pipeline = &mut self.running[pi];
        let job = &mut pipeline.jobs[ji];
        eprintln!(
            "[ci] {} #{} {}: {note}",
            pipeline.repo, pipeline.number, job.def.name
        );
        job.status = JobStatus::Failed;
        job.note = note.clone();
        job.finished = Some(crate::now_secs());
        let name = job.def.name.clone();
        pipeline.save(&self.state_dir, &self.changes);
        self.drop_prefetch(pi, ji);
        let pipeline = self.running[pi].clone();
        let description = self.failure_description(&pipeline, &name, &note);
        self.post(&pipeline, &name, state, description);
    }

    /// A job whose runner stayed away: not the commit's fault, so it is
    /// skipped, and the forge hears success rather than a red cross.
    fn skip_offline(&mut self, pi: usize, ji: usize, runner: &str) {
        let note = format!("runner {runner} offline");
        let pipeline = &mut self.running[pi];
        let job = &mut pipeline.jobs[ji];
        eprintln!(
            "[ci] {} #{} {}: skipped: {note}",
            pipeline.repo, pipeline.number, job.def.name
        );
        job.status = JobStatus::Skipped;
        job.note = note.clone();
        job.finished = Some(crate::now_secs());
        let name = job.def.name.clone();
        pipeline.save(&self.state_dir, &self.changes);
        self.drop_prefetch(pi, ji);
        let pipeline = self.running[pi].clone();
        self.post(
            &pipeline,
            &name,
            State::Success,
            format!("skipped: {note} (#{})", pipeline.number),
        );
    }

    // ------------------------------------------------------------ cancel

    /// Cancels the running pipelines that editors asked to cancel. The
    /// flag is saved before anything is done, so a restart finishes the
    /// cancel. A pipeline that already ended is not running here and the
    /// request is dropped.
    fn cancel_requests(&mut self) {
        for (repo, number) in event::drain_cancels(&self.state_dir) {
            let Some(pi) = self
                .running
                .iter()
                .position(|p| p.repo == repo && p.number == number)
            else {
                continue;
            };
            if !self.running[pi].cancelled {
                self.running[pi].cancelled = true;
                self.running[pi].save(&self.state_dir, &self.changes);
                eprintln!("[ci] {repo} #{number}: cancelled");
            }
            self.apply_cancel(pi);
        }
    }

    /// Kills every active job of a cancelled pipeline and skips the jobs
    /// that never started. A killed job's end is recorded by
    /// [`Scheduler::finish_job`].
    fn apply_cancel(&mut self, pi: usize) {
        for ji in 0..self.running[pi].jobs.len() {
            let job = &self.running[pi].jobs[ji];
            if job.status.is_active() {
                if let Some(runner) = job.runner.clone() {
                    let run = self.run_of(pi, ji);
                    self.runners.send(&runner, ToRunner::Kill { run });
                }
            } else if !job.status.is_terminal() {
                self.cancel_job(pi, ji);
            }
        }
    }

    /// Records a job of a cancelled pipeline as skipped, not failed.
    fn cancel_job(&mut self, pi: usize, ji: usize) {
        let pipeline = &mut self.running[pi];
        let job = &mut pipeline.jobs[ji];
        job.status = JobStatus::Skipped;
        job.note = CANCELLED.to_owned();
        job.finished = Some(crate::now_secs());
        let name = job.def.name.clone();
        pipeline.save(&self.state_dir, &self.changes);
        self.drop_prefetch(pi, ji);
        let pipeline = self.running[pi].clone();
        self.post(
            &pipeline,
            &name,
            State::Error,
            format!("{CANCELLED} (#{})", pipeline.number),
        );
    }

    /// The runner and place of a job whose record predates them.
    fn place_again(&mut self, pi: usize, ji: usize) -> Result<(String, Place), String> {
        let pipeline = &self.running[pi];
        let mirror = mirror_dir(&self.state_dir, &pipeline.repo);
        let sha = pipeline.sha.clone().unwrap_or_default();
        let (runner, place) = place_job(
            &self.config,
            &pipeline.repo,
            &pipeline.jobs[ji].def,
            |image| containerfile_at(&mirror, &sha, image),
        )?;
        let job = &mut self.running[pi].jobs[ji];
        job.runner = Some(runner.clone());
        job.place = Some(place.clone());
        Ok((runner, place))
    }

    /// Hands a job whose needs succeeded to its runner, or records what it
    /// waits for: artifacts, the runner, a free workspace or machine.
    fn assign(&mut self, pi: usize, ji: usize) {
        let now = crate::now_secs();
        let (runner, place) = match (
            &self.running[pi].jobs[ji].runner,
            &self.running[pi].jobs[ji].place,
        ) {
            (Some(runner), Some(place)) => (runner.clone(), place.clone()),
            _ => match self.place_again(pi, ji) {
                Ok(placed) => placed,
                Err(e) => return self.fail_job(pi, ji, e, State::Error),
            },
        };
        let pipeline = self.running[pi].clone();
        let job = &pipeline.jobs[ji];
        let name = job.def.name.clone();

        let key = (pipeline.repo.clone(), pipeline.number, name.clone());
        let ready = *self.ready_since.entry(key.clone()).or_insert(now);

        // Inputs: what a need left, from this runner's own disk when the need
        // ran here, else relayed by the CI from the need's runner, which has
        // to be connected for that.
        let mut inputs = Vec::new();
        for need in &job.def.needs {
            let Some(needed) = pipeline.jobs.iter().find(|j| j.def.name == *need) else {
                continue;
            };
            if !needed.artifacts {
                continue;
            }
            let run = run_id(&pipeline.repo, pipeline.number, need);
            let producer = needed.runner.clone().unwrap_or_default();
            let source = if producer == runner {
                InputSource::Local { run }
            } else {
                if !self.runners.is_connected(&producer) {
                    if !self.config.runners.contains_key(&producer) {
                        return self.fail_job(
                            pi,
                            ji,
                            format!("runner {producer}, which holds {need}'s artifacts, is not configured"),
                            State::Error,
                        );
                    }
                    if now.saturating_sub(ready) > self.wait_secs(&producer) {
                        self.ready_since.remove(&key);
                        self.skip_offline(pi, ji, &producer);
                    } else {
                        let note = format!("waiting for runner {producer} (artifacts of {need})");
                        self.set_waiting(pi, ji, note);
                    }
                    return;
                }
                InputSource::Ci { run }
            };
            inputs.push(Input {
                need: need.clone(),
                source,
            });
        }

        let Some(view) = self.runners.view(&runner) else {
            if !self.config.runners.contains_key(&runner) {
                return self.fail_job(
                    pi,
                    ji,
                    format!("runner {runner} is not configured"),
                    State::Error,
                );
            }
            if now.saturating_sub(ready) > self.wait_secs(&runner) {
                self.ready_since.remove(&key);
                self.skip_offline(pi, ji, &runner);
            } else {
                self.set_waiting(pi, ji, format!("waiting for runner {runner}"));
            }
            return;
        };
        match &place {
            Place::Container { .. } if !view.offer.containers => {
                return self.fail_job(
                    pi,
                    ji,
                    format!("runner {runner} offers no containers"),
                    State::Error,
                );
            }
            Place::Machine { name } if !view.offer.machines.contains(name) => {
                return self.fail_job(
                    pi,
                    ji,
                    format!("runner {runner} offers no machine {name}"),
                    State::Error,
                );
            }
            _ => {}
        }
        if view.busy.is_some_and(|state| state.busy) && job.def.when_busy != WhenBusy::Run {
            return self.set_waiting(pi, ji, format!("runner {runner} busy"));
        }
        if let Some(note) = self.workspace_wait(pi, ji, &runner) {
            return self.set_waiting(pi, ji, note);
        }
        if let Place::Machine { name } = &place
            && self.machine_busy(name)
        {
            return;
        }

        let assignment = match self.build_assignment(&pipeline, ji, &place, inputs) {
            Ok(assignment) => assignment,
            Err(e) => return self.fail_job(pi, ji, e, State::Error),
        };
        let run = assignment.run.clone();
        self.runners.assign(&run, &runner);
        if !self.runners.send(
            &runner,
            ToRunner::Assign {
                assignment: Box::new(assignment),
            },
        ) {
            // The runner dropped between the check and the send.
            self.runners.unassign(&run);
            return;
        }
        self.ready_since.remove(&key);
        let record = &mut self.running[pi].jobs[ji];
        record.status = JobStatus::Starting;
        record.started = Some(now);
        record.assigned = Some(now);
        record.note.clear();
        self.running[pi].save(&self.state_dir, &self.changes);
        let pipeline = self.running[pi].clone();
        self.post(
            &pipeline,
            &name,
            State::Pending,
            format!(
                "starting on {} (#{})",
                place_label(&pipeline.jobs[ji]),
                pipeline.number
            ),
        );
    }

    /// What a runner needs to execute job `ji`: the forge access, the
    /// environment and the secrets granted for the event. A manual run has
    /// the working trees of its upload instead of a commit to fetch, and
    /// neither forge access nor secrets.
    fn build_assignment(
        &self,
        pipeline: &Pipeline,
        ji: usize,
        place: &Place,
        inputs: Vec<Input>,
    ) -> Result<Assignment, String> {
        let job = &pipeline.jobs[ji].def;
        let repo = self.config.repos.get(&pipeline.repo);
        let sha = pipeline.sha.clone().unwrap_or_default();
        let upload = match manual::upload_of(pipeline) {
            Some(id) => {
                let stored = manual::read_upload(&self.state_dir, id)
                    .map_err(|_| format!("manual upload {id} is gone"))?;
                Some(Upload { dirs: stored.dirs })
            }
            None => None,
        };

        let mut secrets = Vec::new();
        let mut fetch_token = None;
        let (url, fetch_user) = match (repo, &upload) {
            (_, Some(_)) => (String::new(), String::new()),
            (Some(repo), None) => {
                // Secrets: only those granted for this event.
                for name in &job.secrets {
                    if !repo.grants(name, &pipeline.event) {
                        return Err(format!(
                            "secret {name} is not granted for {} {}",
                            pipeline.event.kind, pipeline.event.git_ref
                        ));
                    }
                    secrets.push((
                        name.clone(),
                        self.secrets.get(name).map_err(|e| e.to_string())?,
                    ));
                }
                fetch_token = repo
                    .fetch_token
                    .as_ref()
                    .map(|name| self.secrets.get(name).map_err(|e| e.to_string()))
                    .transpose()?;
                (repo.url.clone(), repo.fetch_user.clone())
            }
            (None, None) => {
                return Err(format!("repo {} is no longer configured", pipeline.repo));
            }
        };

        let mut env: Vec<(String, String)> = vec![
            ("CI".into(), "true".into()),
            ("ZEUGHAUS_CI".into(), "1".into()),
            ("CI_REPO".into(), pipeline.repo.clone()),
            (
                "CI_REPO_SLUG".into(),
                repo.map_or_else(String::new, |repo| repo.slug.clone()),
            ),
            ("CI_EVENT".into(), pipeline.event.kind.to_string()),
            ("CI_REF".into(), pipeline.event.git_ref.clone()),
            ("CI_SHA".into(), sha.clone()),
            ("CI_PIPELINE".into(), pipeline.number.to_string()),
            ("CI_JOB".into(), job.name.clone()),
            ("CI_ACTOR".into(), pipeline.event.actor.clone()),
        ];
        if let Some(platform) = &job.platform {
            env.push(("CI_PLATFORM".into(), platform.clone()));
        }
        if let Some(cron) = &pipeline.event.cron {
            env.push(("CI_CRON".into(), cron.clone()));
        }
        env.extend(job.env.iter().map(|(k, v)| (k.clone(), v.clone())));

        Ok(Assignment {
            run: run_id(&pipeline.repo, pipeline.number, &job.name),
            repo: pipeline.repo.clone(),
            number: pipeline.number,
            job: job.name.clone(),
            url,
            fetch_user,
            fetch_token,
            sha,
            file: job.file.clone(),
            interpreter: job.interpreter.clone(),
            place: place.clone(),
            env,
            secrets,
            timeout_minutes: job.timeout_minutes,
            when_busy: job.when_busy,
            inputs,
            checkout: checkout_of(pipeline, ji),
            upload,
        })
    }

    // ------------------------------------------------------------ runner messages

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::RunnerHello {
                name,
                runs,
                artifacts,
            } => self.runner_hello(&name, runs, artifacts),
            Msg::RunnerGone { name } => eprintln!("[ci] runner {name} disconnected"),
            Msg::FromRunner { name, message } => self.on_runner_message(&name, message),
        }
    }

    /// A runner (re)connected and says which runs it knows and which artifact
    /// sets it holds. That settles every job that was active on it, every run
    /// it still holds, and drops every set no job will need: a `DropArtifacts`
    /// lost with a connection or a restart of this process ends here.
    fn runner_hello(&mut self, name: &str, runs: Vec<RunReport>, artifacts: Vec<String>) {
        let Some(view) = self.runners.view(name) else {
            return;
        };
        let reported: HashSet<&str> = runs.iter().map(|r| r.run.as_str()).collect();
        for pi in 0..self.running.len() {
            for ji in 0..self.running[pi].jobs.len() {
                let job = &self.running[pi].jobs[ji];
                if job.runner.as_deref() != Some(name)
                    || !job.status.is_active()
                    || reported.contains(self.run_of(pi, ji).as_str())
                {
                    continue;
                }
                if job.status == JobStatus::Starting {
                    // Assigned after this connection began, the runner has it
                    // in its queue and has not reported it yet.
                    if job.started.is_some_and(|at| at < view.since) {
                        self.requeue(pi, ji);
                    }
                } else {
                    self.runners.unassign(&self.run_of(pi, ji));
                    self.fail_job(pi, ji, format!("runner {name} lost the run"), State::Error);
                }
            }
        }
        for report in runs {
            if let Some((pi, ji)) = self.active_on(name, &report.run) {
                self.runners.assign(&report.run, name);
                if self.running[pi].jobs[ji].status == JobStatus::Starting {
                    // The runner lists it, so it runs; the `Started` was lost.
                    let run = report.run.clone();
                    self.on_runner_message(name, FromRunner::Started { run });
                }
                if let Some(end) = report.end {
                    self.finish_job(pi, ji, end);
                } else if self.running[pi].cancelled {
                    // Cancelled while this runner was away: its kill was lost.
                    self.runners.send(
                        name,
                        ToRunner::Kill {
                            run: report.run.clone(),
                        },
                    );
                }
            } else {
                // A run nobody here waits for: stop it and let it go.
                let run = report.run;
                self.runners.send(name, ToRunner::Kill { run: run.clone() });
                self.runners.unassign(&run);
                self.runners.send(name, ToRunner::Forget { run });
            }
        }
        for run in artifacts {
            if !self.set_wanted(name, &run) {
                self.runners.send(name, ToRunner::DropArtifacts { run });
            }
        }
    }

    /// Whether the artifact set of `run` that `runner` holds may still be
    /// needed: its job has not been settled yet, or a job that needs it has
    /// not ended.
    fn set_wanted(&self, runner: &str, run: &str) -> bool {
        let Some((pi, ji)) = self.locate(run) else {
            return false;
        };
        let pipeline = &self.running[pi];
        let job = &pipeline.jobs[ji];
        job.runner.as_deref() == Some(runner)
            && (!job.status.is_terminal()
                || (job.artifacts && consumers_pending(pipeline, &job.def.name)))
    }

    /// Tells each runner to delete the artifact sets whose consumers have all
    /// ended. A set whose runner is offline stays marked and is dropped when
    /// it is connected again, or at its next hello.
    fn drop_artifacts(&mut self) {
        for pi in 0..self.running.len() {
            let mut dropped = false;
            for ji in 0..self.running[pi].jobs.len() {
                let pipeline = &self.running[pi];
                let job = &pipeline.jobs[ji];
                if !job.artifacts || consumers_pending(pipeline, &job.def.name) {
                    continue;
                }
                let Some(runner) = job.runner.clone() else {
                    continue;
                };
                let run = self.run_of(pi, ji);
                if self.runners.send(&runner, ToRunner::DropArtifacts { run }) {
                    self.running[pi].jobs[ji].artifacts = false;
                    dropped = true;
                }
            }
            if dropped {
                self.running[pi].save(&self.state_dir, &self.changes);
            }
        }
    }

    fn on_runner_message(&mut self, name: &str, message: FromRunner) {
        match message {
            // The enlist service handles both before they get here.
            FromRunner::Hello { .. } | FromRunner::Output { .. } => {}
            FromRunner::Busy { state } => self.runners.set_busy(name, state),
            FromRunner::Machine {
                name: machine,
                status,
            } => self.runners.set_machine(name, &machine, &status),
            FromRunner::Started { run } => {
                let Some((pi, ji)) = self.active_on(name, &run) else {
                    return;
                };
                let record = &mut self.running[pi].jobs[ji];
                record.status = JobStatus::Running;
                let now = crate::now_secs();
                record.started = Some(now);
                // The runner stages every input before it starts the program:
                // a transfer still open lost the report of its end.
                for transfer in record.transfers.iter_mut().filter(|t| t.finished.is_none()) {
                    transfer.finished = Some(now);
                }
                record.note.clear();
                let job = record.def.name.clone();
                self.running[pi].save(&self.state_dir, &self.changes);
                let pipeline = self.running[pi].clone();
                self.post(
                    &pipeline,
                    &job,
                    State::Pending,
                    format!(
                        "running on {} (#{})",
                        place_label(&pipeline.jobs[ji]),
                        pipeline.number
                    ),
                );
            }
            FromRunner::StartFailed { run, message } => {
                let Some((pi, ji)) = self.active_on(name, &run) else {
                    return;
                };
                self.runners.unassign(&run);
                self.fail_job(pi, ji, message, State::Error);
            }
            FromRunner::Frozen { run, frozen } => {
                let Some((pi, ji)) = self.active_on(name, &run) else {
                    return;
                };
                let target = if frozen {
                    JobStatus::Frozen
                } else {
                    JobStatus::Running
                };
                let record = &mut self.running[pi].jobs[ji];
                if record.status == target || record.status == JobStatus::Starting {
                    return;
                }
                record.status = target;
                record.note = if frozen {
                    "paused: runner busy".to_owned()
                } else {
                    String::new()
                };
                let job = record.def.name.clone();
                self.running[pi].save(&self.state_dir, &self.changes);
                let pipeline = self.running[pi].clone();
                let description = if frozen {
                    format!("paused: runner busy (#{})", pipeline.number)
                } else {
                    format!(
                        "running on {} (#{})",
                        place_label(&pipeline.jobs[ji]),
                        pipeline.number
                    )
                };
                self.post(&pipeline, &job, State::Pending, description);
            }
            FromRunner::Finished { run, end } => {
                if let Some((pi, ji)) = self.active_on(name, &run) {
                    self.finish_job(pi, ji, end);
                } else if self.locate(&run).is_some_and(|(pi, ji)| {
                    self.running[pi].jobs[ji].runner.as_deref() == Some(name)
                }) {
                    // Sent again after a reconnect: already settled.
                    self.runners.send(name, ToRunner::Forget { run });
                }
            }
            FromRunner::Transfer { run, need, event } => {
                self.on_transfer(name, &run, need, event);
            }
        }
    }

    /// Records how far an input moves to the runner of the job `run`, and
    /// announces the record. Ignored unless that runner is the job's, the job
    /// has not ended and `need` is one of its needs: the record is what
    /// editors show.
    fn on_transfer(&mut self, name: &str, run: &str, need: String, event: TransferEvent) {
        let Some((pi, ji)) = self.locate(run) else {
            return;
        };
        let job = &mut self.running[pi].jobs[ji];
        if job.runner.as_deref() != Some(name)
            || job.status.is_terminal()
            || !job.def.needs.contains(&need)
        {
            return;
        }
        let now = crate::now_secs();
        match event {
            TransferEvent::Started => {
                // An attempt that never reported its end is superseded.
                job.transfers
                    .retain(|t| t.need != need || t.finished.is_some());
                job.transfers.push(TransferRecord {
                    need,
                    started: now,
                    finished: None,
                    bytes: None,
                    error: None,
                });
            }
            TransferEvent::Done { bytes } => {
                let Some(transfer) = open_transfer(job, &need) else {
                    return;
                };
                transfer.finished = Some(now);
                transfer.bytes = Some(bytes);
            }
            TransferEvent::Failed { message } => {
                let Some(transfer) = open_transfer(job, &need) else {
                    return;
                };
                transfer.finished = Some(now);
                transfer.error = Some(message);
            }
        }
        self.running[pi].save(&self.state_dir, &self.changes);
    }

    /// A run ended. Its transcript is complete here: the runner streams the
    /// log to its end before it says so.
    fn finish_job(&mut self, pi: usize, ji: usize, end: RunEnd) {
        let now = crate::now_secs();
        let pipeline = &self.running[pi];
        let record = &pipeline.jobs[ji];
        let (repo, number, job) = (
            pipeline.repo.clone(),
            pipeline.number,
            record.def.name.clone(),
        );
        let run = run_id(&repo, number, &job);
        let elapsed = now.saturating_sub(record.started.unwrap_or(now));
        let timeout_minutes = record.def.timeout_minutes;
        let record_runner = record.runner.clone();
        let dir = transcript_dir(&self.state_dir, &repo, number, &job);
        let succeeded = end.code == Some(0);

        let exit = format!(
            "code={}\n",
            end.code
                .map_or_else(|| "none".to_owned(), |c| c.to_string())
        );
        if let Err(e) = crate::files::write_atomic(&dir.join("exit"), exit.as_bytes()) {
            eprintln!("[ci] {repo} #{number} {job}: {e}");
        }
        let (state, note, excerpt) = if succeeded {
            (
                State::Success,
                format!("passed in {}m{}s", elapsed / 60, elapsed % 60),
                None,
            )
        } else if self.running[pi].cancelled {
            (State::Error, CANCELLED.to_owned(), None)
        } else {
            (
                State::Failure,
                failure_note(&end, timeout_minutes),
                excerpt::extract(&dir),
            )
        };

        let cancelled = !succeeded && self.running[pi].cancelled;
        let record = &mut self.running[pi].jobs[ji];
        record.status = if succeeded {
            JobStatus::Succeeded
        } else if cancelled {
            JobStatus::Skipped
        } else {
            JobStatus::Failed
        };
        record.code = end.code;
        record.note = note.clone();
        record.finished = Some(now);
        record.excerpt = excerpt;
        record.artifacts = succeeded && end.artifacts;
        eprintln!("[ci] {repo} #{number} {job}: {note}");
        self.running[pi].save(&self.state_dir, &self.changes);
        if let Some(runner) = &record_runner
            && self.running[pi].jobs[ji].artifacts
        {
            self.prefetch_for_consumers(pi, ji, runner);
        }
        let pipeline = self.running[pi].clone();
        let description = if succeeded || cancelled {
            format!("{note} (#{number})")
        } else {
            self.failure_description(&pipeline, &job, &note)
        };
        self.post(&pipeline, &job, state, description);

        if let Some(runner) = record_runner {
            self.runners
                .send(&runner, ToRunner::Forget { run: run.clone() });
        }
        self.runners.unassign(&run);
    }

    /// Tells the runner of every job that waits for the set job `ji` just
    /// left, and is placed on another runner than `producer`, to fetch it
    /// now: that job is assigned only when its other needs have ended, and
    /// the set moves meanwhile. A runner that is not connected is not told;
    /// its job fetches when it starts.
    fn prefetch_for_consumers(&self, pi: usize, ji: usize, producer: &str) {
        let pipeline = &self.running[pi];
        let need = &pipeline.jobs[ji].def.name;
        let run = run_id(&pipeline.repo, pipeline.number, need);
        for consumer in &pipeline.jobs {
            let waiting = matches!(consumer.status, JobStatus::Pending | JobStatus::Waiting);
            let Some(runner) = consumer.runner.as_deref() else {
                continue;
            };
            if !waiting || runner == producer || !consumer.def.needs.contains(need) {
                continue;
            }
            self.runners.send(
                runner,
                ToRunner::Prefetch {
                    run: run.clone(),
                    consumer: run_id(&pipeline.repo, pipeline.number, &consumer.def.name),
                    need: need.clone(),
                },
            );
        }
    }

    /// Tells the runner of job `ji`, which ended without being handed over,
    /// to delete what it fetched early for it: nothing will ask for it now.
    fn drop_prefetch(&self, pi: usize, ji: usize) {
        let job = &self.running[pi].jobs[ji];
        if job.assigned.is_some() || job.transfers.is_empty() {
            return;
        }
        if let Some(runner) = &job.runner {
            let consumer = self.run_of(pi, ji);
            self.runners
                .send(runner, ToRunner::DropPrefetch { consumer });
        }
    }

    // ------------------------------------------------------------ red default branch

    /// The streak of the repository's default branch, when `pipeline` ran
    /// for that branch. Read from the records, `pipeline`'s saved state
    /// included.
    fn branch_streak(&self, pipeline: &Pipeline) -> Option<Streak> {
        let branch = &self.config.repos.get(&pipeline.repo)?.default_branch;
        streak::is_branch(pipeline, branch).then(|| {
            streak::of_branch(
                &load_repo_pipelines(&self.state_dir, &pipeline.repo),
                branch,
            )
        })
    }

    /// What the forge shows for a failed job: the cause line of its
    /// excerpt in place of the bare note, and the streak when the default
    /// branch was red for this job before this pipeline.
    fn failure_description(&self, pipeline: &Pipeline, job: &str, note: &str) -> String {
        let mut text = pipeline
            .jobs
            .iter()
            .find(|j| j.def.name == job)
            .and_then(shareable_cause)
            .unwrap_or_else(|| note.to_owned());
        if let Some(red) = self
            .branch_streak(pipeline)
            .and_then(|s| s.red.into_iter().find(|r| r.job == job))
            && red.since < pipeline.number
        {
            text = format!("red since #{}: {text}", red.since);
        }
        forge::describe(&text, pipeline.number)
    }

    /// Tells the user when the default branch turns red, and again for
    /// every red pipeline once a job has been red for [`RED_REMINDER`]: in
    /// the journal, and on the desktop of every editor connected.
    fn alert_red(&self, pipeline: &Pipeline) {
        let Some(streak) = self.branch_streak(pipeline) else {
            return;
        };
        let now = crate::now_secs();
        let mut due = false;
        let mut lines = Vec::new();
        for job in pipeline
            .jobs
            .iter()
            .filter(|j| j.status == JobStatus::Failed)
        {
            let Some(red) = streak.red.iter().find(|r| r.job == job.def.name) else {
                continue;
            };
            due |= red.since == pipeline.number
                || now.saturating_sub(red.since_created) >= RED_REMINDER.as_secs();
            let mut line = red.describe(now);
            if let Some(cause) = shareable_cause(job) {
                // One log line can be a megabyte; a notification is read
                // at a glance.
                let cause: String = cause.chars().take(ALERT_CAUSE_CHARS).collect();
                line = format!("{line}: {cause}");
            }
            lines.push(line);
        }
        if !due || lines.is_empty() {
            return;
        }
        if let Some((number, jobs)) = &streak.last_deploy {
            lines.push(format!("last {} #{number}", jobs.join(", ")));
        }
        let title = format!(
            "{} {} is red (#{})",
            pipeline.repo, pipeline.event.git_ref, pipeline.number
        );
        eprintln!("[ci] {title}: {}", lines.join("; "));
        let _ = self.alerts.send(Alert {
            title,
            body: lines.join("\n"),
        });
    }

    // ------------------------------------------------------------ lost runners

    /// Settles jobs whose runner is not connected. A run goes on at a runner
    /// that reconnects, which reports it in its `Hello`; one that stays away
    /// past the job's timeout and a grace period is given up, and a job that
    /// was only handed over goes back to waiting for its runner.
    fn lost_runners(&mut self) {
        let now = crate::now_secs();
        for pi in 0..self.running.len() {
            for ji in 0..self.running[pi].jobs.len() {
                let job = &self.running[pi].jobs[ji];
                let Some(runner) = job.runner.clone() else {
                    continue;
                };
                if !job.status.is_active() || self.runners.is_connected(&runner) {
                    continue;
                }
                let started = job.started.unwrap_or(now);
                if job.status == JobStatus::Starting {
                    if now > started + self.wait_secs(&runner) {
                        self.requeue(pi, ji);
                    }
                } else if now > started + u64::from(job.def.timeout_minutes) * 60 + LOST_GRACE_SECS
                {
                    self.runners.unassign(&self.run_of(pi, ji));
                    self.fail_job(pi, ji, format!("runner {runner} lost"), State::Error);
                }
            }
        }
    }

    /// Seconds a job waits for `runner` to connect.
    fn wait_secs(&self, runner: &str) -> u64 {
        self.config
            .runners
            .get(runner)
            .map_or(15, |r| u64::from(r.wait_minutes))
            * 60
    }

    /// Puts a job that never started back to pending, for the next
    /// [`Scheduler::advance`] to place again.
    fn requeue(&mut self, pi: usize, ji: usize) {
        let run = self.run_of(pi, ji);
        self.runners.unassign(&run);
        let job = &mut self.running[pi].jobs[ji];
        job.status = JobStatus::Pending;
        job.note.clear();
        job.started = None;
        job.assigned = None;
        job.transfers.clear();
        self.running[pi].save(&self.state_dir, &self.changes);
    }
}

/// The newest transfer of `need` that has not ended.
fn open_transfer<'a>(job: &'a mut JobRecord, need: &str) -> Option<&'a mut TransferRecord> {
    job.transfers
        .iter_mut()
        .rev()
        .find(|t| t.need == need && t.finished.is_none())
}

/// The cause line of a failed job's excerpt, unless the job holds secrets:
/// its excerpt is its own output, and a status description or an alert is
/// read by people the secrets are not for.
fn shareable_cause(job: &JobRecord) -> Option<String> {
    if !job.def.secrets.is_empty() {
        return None;
    }
    job.cause()
}

fn failure_note(end: &RunEnd, timeout_minutes: u32) -> String {
    match end.code {
        _ if end.timed_out => format!("timed out after {timeout_minutes} min"),
        Some(70) => "failed: checkout (exit 70)".to_owned(),
        Some(71) => "failed: container image build (exit 71)".to_owned(),
        Some(74) => "failed: copying artifacts from the guest (exit 74)".to_owned(),
        Some(75) => "failed: guest unreachable (exit 75)".to_owned(),
        Some(code) => format!("failed with exit {code}"),
        None if end.killed => "killed".to_owned(),
        None => "died from a signal".to_owned(),
    }
}

/// `host`, `image:<x>`, `machine:<x>`, `platform:<p>` or `platform:<p> image:<x>`.
pub fn place_name(job: &JobDef) -> String {
    match (&job.platform, &job.image, &job.machine) {
        (Some(platform), Some(image), _) => format!("platform:{platform} image:{image}"),
        (Some(platform), None, _) => format!("platform:{platform}"),
        (None, Some(image), _) => format!("image:{image}"),
        (None, None, Some(machine)) => format!("machine:{machine}"),
        (None, None, None) => "host".to_owned(),
    }
}

/// Decides where a job runs: the runner and the place on it. A pure function
/// of the config and the job, so a bad `machine` or `platform` fails the
/// pipeline when it is set up rather than when its job is assigned.
/// `containerfile` reads the text of `<image>.Containerfile` at the pipeline's
/// commit; the image tag follows that text, so a changed Containerfile builds
/// a new image.
///
/// The target is the runner or machine the job names: `ci.toml`'s
/// `[platforms]` entry of its `platform`, else its `machine`. An image runs
/// in a container on the target runner, or on the default runner when there
/// is no target.
pub fn place_job(
    config: &CiConfig,
    repo: &str,
    job: &JobDef,
    containerfile: impl Fn(&str) -> Result<String, String>,
) -> Result<(String, Place), String> {
    /// What the job's `platform` or `machine` names.
    enum Target<'a> {
        /// Neither is set.
        Default,
        Runner(&'a str),
        Machine {
            name: &'a str,
            runner: &'a str,
        },
    }

    let fail = |msg: String| format!("{CI_DIR}/{}: {msg}", job.file);
    let (name, what) = match (&job.platform, &job.machine) {
        (Some(platform), _) => {
            let target = config.platforms.get(platform).ok_or_else(|| {
                fail(format!("platform {platform} is not in ci.toml [platforms]"))
            })?;
            (Some(target), format!("platform {platform} ({target})"))
        }
        (None, Some(machine)) => (Some(machine), format!("machine {machine}")),
        (None, None) => (None, "the default placement".to_owned()),
    };
    let target = match name {
        None => Target::Default,
        Some(name) if config.runners.contains_key(name) => Target::Runner(name),
        Some(name) => match config.runner_of_machine(name) {
            Some(runner) => Target::Machine { name, runner },
            None => {
                return Err(fail(format!(
                    "{what} is neither a runner nor a machine of one"
                )));
            }
        },
    };
    if let Some(image) = &job.image {
        let runner = match target {
            Target::Default => &config.default_runner,
            Target::Runner(runner) => runner,
            Target::Machine { .. } => {
                return Err(fail(format!(
                    "{what} is a Windows VM; an image needs a runner"
                )));
            }
        };
        if config.runner_os(runner) == RunnerOs::Windows {
            return Err(fail(format!(
                "{what} is the Windows runner {runner}; an image needs a unix runner"
            )));
        }
        let text = containerfile(image)?;
        let digest = hex::encode(Sha256::digest(text.as_bytes()));
        let tag = format!("localhost/zci-{repo}-{image}:{}", &digest[..16]);
        return Ok((
            runner.to_owned(),
            Place::Container {
                image: image.clone(),
                tag,
            },
        ));
    }
    let is_ps1 = job.file.ends_with(".ps1");
    let (runner, place) = match target {
        Target::Default => (config.default_runner.as_str(), Place::Host),
        Target::Runner(runner) => (runner, Place::Host),
        Target::Machine { name, runner } => {
            if !is_ps1 {
                return Err(fail(format!(
                    "{what} is a Windows VM; it runs only .ps1 jobs"
                )));
            }
            (
                runner,
                Place::Machine {
                    name: name.to_owned(),
                },
            )
        }
    };
    if place == Place::Host {
        match (config.runner_os(runner), is_ps1) {
            (RunnerOs::Unix, true) => {
                return Err(fail(format!(
                    "{what} is a runner; a .ps1 job cannot run there"
                )));
            }
            (RunnerOs::Windows, false) => {
                return Err(fail(format!(
                    "{what} is the Windows runner {runner}; it runs only .ps1 jobs"
                )));
            }
            _ => {}
        }
    }
    if place == Place::Host && job.when_busy == WhenBusy::Freeze {
        return Err(fail(format!(
            "{what} runs on the host of runner {runner}; when_busy = \"freeze\" needs a container or a machine"
        )));
    }
    Ok((runner.to_owned(), place))
}

/// The text of `<image>.Containerfile` at `sha`.
fn containerfile_at(mirror: &Path, sha: &str, image: &str) -> Result<String, String> {
    git(
        mirror,
        &["show", &format!("{sha}:{CI_DIR}/{image}.Containerfile")],
        &[],
    )
}

/// Fetches a manual run's first bundle into the bare repository `mirror`,
/// made on first use: the `.zeughaus-ci/` of the snapshot can then be read
/// from it.
fn fetch_manual_bundle(mirror: &Path, bundle: &Path) -> Result<(), String> {
    if !mirror.join("HEAD").exists() {
        std::fs::create_dir_all(mirror)
            .map_err(|e| format!("cannot create {}: {e}", mirror.display()))?;
        git(mirror, &["init", "--bare", "-q"], &[])?;
    }
    git(
        mirror,
        &[
            "fetch",
            "-q",
            &bundle.to_string_lossy(),
            "+refs/zci-manual/*:refs/zci-manual/*",
        ],
        &[],
    )
    .map(|_| ())
}

/// Where a job runs, for people: its runner, then the container image or
/// machine on it.
fn place_label(job: &JobRecord) -> String {
    let runner = job.runner.as_deref().unwrap_or("?");
    match &job.place {
        Some(Place::Container { image, .. }) => format!("{runner}, image:{image}"),
        Some(Place::Machine { name }) => format!("{runner}, machine:{name}"),
        Some(Place::Host) | None => runner.to_owned(),
    }
}

/// How the launcher treats the workspace of job `ji`: it starts it from
/// nothing where a clean channel's pipeline first uses it, else cleans it and
/// keeps the ignored files a build left.
fn checkout_of(pipeline: &Pipeline, ji: usize) -> Checkout {
    let job = &pipeline.jobs[ji];
    let workspace = job.place.as_ref().map(Place::workspace);
    let used = pipeline.jobs[..ji].iter().any(|earlier| {
        earlier.started.is_some()
            && earlier.runner == job.runner
            && earlier.place.as_ref().map(Place::workspace) == workspace
    });
    if pipeline.clean && !used {
        Checkout::Wipe
    } else {
        Checkout::Clean
    }
}

/// Whether a job of `pipeline` that needs `job` has not ended yet.
fn consumers_pending(pipeline: &Pipeline, job: &str) -> bool {
    pipeline
        .jobs
        .iter()
        .any(|j| j.def.needs.iter().any(|need| need == job) && !j.status.is_terminal())
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

/// The commit an event names. A forge reports an annotated tag's push with
/// the tag object's sha, which is no commit: jobs check out, statuses and
/// releases point at the commit it peels to.
fn event_commit(mirror: &Path, event: &CiEvent) -> Result<String, String> {
    match &event.sha {
        Some(sha) => rev_parse(mirror, &format!("{sha}^{{commit}}"))
            .map_err(|_| format!("commit {sha} is not in the mirror")),
        None => {
            let refname = match event.kind {
                EventKind::Tag => format!("refs/tags/{}^{{commit}}", event.git_ref),
                EventKind::Push | EventKind::Cron => {
                    format!("refs/heads/{}^{{commit}}", event.git_ref)
                }
                EventKind::Manual => return Err("a manual event carries its sha".to_owned()),
            };
            rev_parse(mirror, &refname)
        }
    }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use zeughaus_link::enlist::UploadDir;
    use zeughaus_link::manual::RunUpload;

    use super::*;

    const FP_1: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    const FP_2: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";
    const FP_3: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000003";

    #[test]
    fn subjects_parse_with_empty_and_spaced_text() {
        let map = parse_subjects("aaa\0fix: a  b\nbbb\0\nccc\0 x \n");
        assert_eq!(map["aaa"], "fix: a  b");
        assert_eq!(map["bbb"], "");
        assert_eq!(map["ccc"], "x");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeughaus-sched-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    fn pipeline(channel: &str) -> Pipeline {
        Pipeline {
            repo: "view".to_owned(),
            number: 7,
            key: String::new(),
            event: CiEvent {
                repo: "view".to_owned(),
                kind: EventKind::Push,
                git_ref: "main".to_owned(),
                sha: None,
                cron: None,
                actor: "test".to_owned(),
                delivery: None,
                upload: None,
                received: 0,
            },
            sha: None,
            subject: String::new(),
            status: PipelineStatus::Running,
            note: String::new(),
            created: 0,
            finished: None,
            jobs: Vec::new(),
            channel: channel.to_owned(),
            cancelled: false,
            clean: false,
        }
    }

    #[test]
    fn an_annotated_tag_event_runs_the_commit_it_points_at() {
        let repo = scratch("peel");
        let run = |args: &[&str]| {
            git(
                &repo,
                args,
                &[
                    ("GIT_AUTHOR_NAME".to_owned(), "t".to_owned()),
                    ("GIT_AUTHOR_EMAIL".to_owned(), "t@t".to_owned()),
                    ("GIT_COMMITTER_NAME".to_owned(), "t".to_owned()),
                    ("GIT_COMMITTER_EMAIL".to_owned(), "t@t".to_owned()),
                ],
            )
            .expect("git")
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", "release"]);
        run(&["tag", "-a", "v1.0.0", "-m", "Release 1.0.0"]);
        let commit = run(&["rev-parse", "HEAD"]).trim().to_owned();
        let tag_object = run(&["rev-parse", "v1.0.0"]).trim().to_owned();
        assert_ne!(
            commit, tag_object,
            "an annotated tag is an object of its own"
        );

        let mut event = pipeline("release").event;
        event.kind = EventKind::Tag;
        event.git_ref = "v1.0.0".to_owned();
        // As the forge reports the push, and as `zeughaus-ci run` names it.
        event.sha = Some(tag_object);
        assert_eq!(event_commit(&repo, &event), Ok(commit.clone()));
        event.sha = None;
        assert_eq!(event_commit(&repo, &event), Ok(commit));

        event.sha = Some("0".repeat(40));
        assert!(event_commit(&repo, &event).is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_saved_record_is_announced_under_its_channel() {
        let state = scratch("announce");
        let (changes, announced) = std::sync::mpsc::channel();
        pipeline("release").save(&state, &changes);
        // A record from before channels existed is announced under the
        // channel its event sorts into.
        let mut old = pipeline("");
        old.number = 8;
        old.save(&state, &changes);

        assert!(pipelines_dir(&state, "view").join("7.json").exists());
        assert_eq!(
            announced.try_recv(),
            Ok(CiChange {
                repo: "view".to_owned(),
                channel: "release".to_owned(),
                number: 7,
            })
        );
        assert_eq!(
            announced.try_recv(),
            Ok(CiChange {
                repo: "view".to_owned(),
                channel: "main".to_owned(),
                number: 8,
            })
        );
        assert!(announced.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_record_that_cannot_be_written_is_not_announced() {
        let state = scratch("unwritable");
        // A state directory below a file cannot be created.
        let file = state.join("file");
        std::fs::write(&file, "").expect("write the blocking file");
        let (changes, announced) = std::sync::mpsc::channel();
        pipeline("dev").save(&file.join("state"), &changes);
        assert!(announced.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_record_from_before_enlisting_still_loads() {
        let old = r#"{"def":{"name":"a","file":"a.sh","on":[],"needs":[],"image":null,
            "machine":null,"cache":[],"secrets":[],"when_busy":"wait","env":{},
            "timeout_minutes":60,"interpreter":"sh"},"status":"succeeded",
            "run_dir":"/x/runs/3","active_secs":4,"image_tag":null}"#;
        let record: JobRecord = serde_json::from_str(old).expect("an old record loads");
        assert_eq!(record.runner, None);
        assert_eq!(record.place, None);
        assert!(!record.artifacts);
    }

    /// Records on disk outlive the code that wrote them: those of jobs that
    /// had a `context` and `cache` load, and the keys go.
    #[test]
    fn a_job_record_that_carries_context_and_cache_still_loads() {
        let mut json = serde_json::to_value(record("a", &[], "x", JobStatus::Succeeded)).unwrap();
        let def = json["def"].as_object_mut().unwrap();
        def.insert("context".to_owned(), "linux".into());
        def.insert(
            "cache".to_owned(),
            serde_json::json!([".ci-cache", "target"]),
        );
        let old: JobRecord = serde_json::from_value(json).expect("an old record loads");
        assert_eq!(old.def.name, "a");
        assert!(old.def.ignored_keys.is_empty());
        let saved = serde_json::to_value(&old).unwrap();
        assert!(saved["def"].get("context").is_none() && saved["def"].get("cache").is_none());
    }

    fn config() -> CiConfig {
        CiConfig::parse(&format!(
            "default_runner = \"tuco-pc\"\n\
             [runners.tuco-pc]\nfingerprint = \"{FP_1}\"\nmachines = [\"win11\"]\n\
             [runners.atik]\nfingerprint = \"{FP_2}\"\n\
             [runners.wohnzimmer]\nfingerprint = \"{FP_3}\"\nos = \"windows\"\n\
             [platforms]\nlinux-amd64 = \"tuco-pc\"\nlinux-arm64 = \"atik\"\n\
             windows-amd64 = \"win11\"\n\
             [repos.demo]\nurl = \"git@example.org:me/demo.git\"\n"
        ))
        .expect("a valid config")
    }

    fn job(
        file: &str,
        image: Option<&str>,
        machine: Option<&str>,
        platform: Option<&str>,
    ) -> JobDef {
        JobDef {
            name: "j".to_owned(),
            file: file.to_owned(),
            on: vec!["push *".to_owned()],
            needs: Vec::new(),
            image: image.map(str::to_owned),
            machine: machine.map(str::to_owned),
            platform: platform.map(str::to_owned),
            ignored_keys: Vec::new(),
            channels: Vec::new(),
            secrets: Vec::new(),
            when_busy: WhenBusy::Wait,
            env: Default::default(),
            timeout_minutes: 60,
            interpreter: "sh".to_owned(),
        }
    }

    fn containerfile(image: &str) -> Result<String, String> {
        Ok(format!("FROM {image}\n"))
    }

    #[test]
    fn a_runner_named_as_machine_runs_the_job_on_its_host() {
        let placed = place_job(
            &config(),
            "r",
            &job("a.sh", None, Some("atik"), None),
            containerfile,
        );
        assert_eq!(placed, Ok(("atik".to_owned(), Place::Host)));
    }

    #[test]
    fn a_machine_of_a_runner_runs_on_that_machine() {
        let placed = place_job(
            &config(),
            "r",
            &job("w.ps1", None, Some("win11"), None),
            containerfile,
        );
        assert_eq!(
            placed,
            Ok((
                "tuco-pc".to_owned(),
                Place::Machine {
                    name: "win11".to_owned()
                }
            ))
        );
    }

    #[test]
    fn an_image_runs_in_a_container_on_the_default_runner() {
        let (runner, place) = place_job(
            &config(),
            "r",
            &job("a.sh", Some("arch"), None, None),
            containerfile,
        )
        .unwrap();
        assert_eq!(runner, "tuco-pc");
        let Place::Container { image, tag } = place else {
            panic!("not a container place");
        };
        assert_eq!(image, "arch");
        let (stem, digest) = tag.split_once(':').unwrap();
        assert_eq!(stem, "localhost/zci-r-arch");
        assert_eq!(digest.len(), 16);
        // The tag follows the Containerfile's content.
        let (_, other) = place_job(
            &config(),
            "r",
            &job("a.sh", Some("arch"), None, None),
            |_| Ok("FROM other\n".to_owned()),
        )
        .unwrap();
        assert_ne!(Place::Container { image, tag }, other);
    }

    #[test]
    fn no_machine_and_no_image_runs_on_the_default_runner() {
        let placed = place_job(
            &config(),
            "r",
            &job("a.sh", None, None, None),
            containerfile,
        );
        assert_eq!(placed, Ok(("tuco-pc".to_owned(), Place::Host)));
    }

    #[test]
    fn a_job_that_does_not_fit_its_machine_is_an_error() {
        let cases = [
            (
                job("a.ps1", None, Some("atik"), None),
                "a .ps1 job cannot run there",
            ),
            (
                job("a.sh", None, Some("win11"), None),
                "runs only .ps1 jobs",
            ),
            (
                job("a.sh", None, Some("nowhere"), None),
                "machine nowhere is neither a runner nor a machine of one",
            ),
        ];
        for (def, expected) in cases {
            let err = place_job(&config(), "r", &def, containerfile).unwrap_err();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_windows_runner_runs_only_ps1_jobs_on_its_host() {
        let placed = place_job(
            &config(),
            "r",
            &job("w.ps1", None, Some("wohnzimmer"), None),
            containerfile,
        );
        assert_eq!(placed, Ok(("wohnzimmer".to_owned(), Place::Host)));
        let cases = [
            (
                job("a.sh", None, Some("wohnzimmer"), None),
                "Windows runner wohnzimmer; it runs only .ps1 jobs",
            ),
            (
                job("a.sh", Some("arch"), Some("wohnzimmer"), None),
                "an image needs a unix runner",
            ),
        ];
        for (def, expected) in cases {
            let err = place_job(&config(), "r", &def, containerfile).unwrap_err();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_platform_on_a_runner_runs_on_its_host() {
        let placed = place_job(
            &config(),
            "r",
            &job("a.sh", None, None, Some("linux-arm64")),
            containerfile,
        );
        assert_eq!(placed, Ok(("atik".to_owned(), Place::Host)));
    }

    #[test]
    fn a_platform_on_a_machine_runs_on_that_machine() {
        let placed = place_job(
            &config(),
            "r",
            &job("w.ps1", None, None, Some("windows-amd64")),
            containerfile,
        );
        assert_eq!(
            placed,
            Ok((
                "tuco-pc".to_owned(),
                Place::Machine {
                    name: "win11".to_owned()
                }
            ))
        );
    }

    #[test]
    fn a_platform_with_an_image_runs_the_container_on_the_platforms_runner() {
        let (runner, place) = place_job(
            &config(),
            "r",
            &job("a.sh", Some("arch"), None, Some("linux-arm64")),
            containerfile,
        )
        .unwrap();
        assert_eq!(runner, "atik");
        assert!(matches!(place, Place::Container { .. }));
    }

    #[test]
    fn a_platform_that_cannot_be_placed_is_an_error() {
        let mut frozen = job("a.sh", None, None, Some("linux-arm64"));
        frozen.when_busy = WhenBusy::Freeze;
        let cases = [
            (
                job("a.sh", None, None, Some("plan9")),
                "platform plan9 is not in ci.toml [platforms]",
            ),
            (
                job("a.sh", Some("arch"), None, Some("windows-amd64")),
                "platform windows-amd64 (win11) is a Windows VM; an image needs a runner",
            ),
            (
                frozen,
                "platform linux-arm64 (atik) runs on the host of runner atik; when_busy = \"freeze\" needs a container or a machine",
            ),
        ];
        for (def, expected) in cases {
            let err = place_job(&config(), "r", &def, containerfile).unwrap_err();
            assert!(err.starts_with(".zeughaus-ci/a.sh: "), "{err}");
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn place_names_show_the_platform_and_its_image() {
        assert_eq!(place_name(&job("a.sh", None, None, None)), "host");
        assert_eq!(
            place_name(&job("a.sh", None, None, Some("linux-arm64"))),
            "platform:linux-arm64"
        );
        assert_eq!(
            place_name(&job("a.sh", Some("arch"), None, Some("linux-arm64"))),
            "platform:linux-arm64 image:arch"
        );
    }

    #[test]
    fn failure_notes_name_the_cause() {
        let end = |code, killed, timed_out| RunEnd {
            code,
            killed,
            timed_out,
            artifacts: false,
        };
        assert_eq!(
            failure_note(&end(Some(124), false, true), 30),
            "timed out after 30 min"
        );
        assert_eq!(
            failure_note(&end(Some(70), false, false), 30),
            "failed: checkout (exit 70)"
        );
        assert_eq!(
            failure_note(&end(Some(3), false, false), 30),
            "failed with exit 3"
        );
        assert_eq!(failure_note(&end(None, true, false), 30), "killed");
        assert_eq!(
            failure_note(&end(None, false, false), 30),
            "died from a signal"
        );
    }

    // ---- assignment and runner messages, against a fake registry

    fn scheduler(state: &Path) -> Scheduler {
        let config = Arc::new(
            CiConfig::parse(&format!(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"{FP_1}\"\n\
                 [runners.y]\nfingerprint = \"{FP_2}\"\nwait_minutes = 1\n\
                 [repos.view]\nurl = \"file:///nowhere\"\n"
            ))
            .expect("a valid config"),
        );
        scheduler_over(state, config)
    }

    fn scheduler_over(state: &Path, config: Arc<CiConfig>) -> Scheduler {
        let (_tx, rx) = std::sync::mpsc::channel();
        let (alerts, _) = std::sync::mpsc::channel();
        let (changes, _) = std::sync::mpsc::channel();
        Scheduler::new(
            state.to_path_buf(),
            config,
            Secrets::new(None, state),
            Arc::new(Runners::new()),
            rx,
            alerts,
            changes,
        )
    }

    fn connect(
        runners: &Runners,
        name: &str,
        since: u64,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ToRunner> {
        connect_offering(runners, name, since, false)
    }

    /// Like [`connect`], for a runner that does (or does not) run containers.
    fn connect_offering(
        runners: &Runners,
        name: &str,
        since: u64,
        containers: bool,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ToRunner> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        runners.insert(
            name,
            crate::runners::Connected {
                tx,
                offer: zeughaus_link::enlist::Offer {
                    os: "linux".to_owned(),
                    arch: "x86_64".to_owned(),
                    cpus: 2,
                    containers,
                    machines: Vec::new(),
                },
                busy: None,
                machines: BTreeMap::new(),
                since,
            },
        );
        rx
    }

    fn record(name: &str, needs: &[&str], runner: &str, status: JobStatus) -> JobRecord {
        let mut def = job(&format!("{name}.sh"), None, None, None);
        def.name = name.to_owned();
        def.needs = needs.iter().map(|n| (*n).to_owned()).collect();
        if !needs.is_empty() {
            def.on.clear();
        }
        JobRecord {
            def,
            status,
            note: String::new(),
            code: None,
            started: None,
            assigned: None,
            transfers: Vec::new(),
            finished: None,
            runner: Some(runner.to_owned()),
            place: Some(Place::Host),
            artifacts: false,
            excerpt: None,
        }
    }

    /// `a` (on `a_runner`) succeeded with artifacts; `b` needs it, runs on `y`.
    fn two_jobs(a_runner: &str) -> Pipeline {
        let mut p = pipeline("dev");
        p.repo = "view".to_owned();
        p.sha = Some("abc".to_owned());
        let mut a = record("a", &[], a_runner, JobStatus::Succeeded);
        a.artifacts = true;
        p.jobs = vec![a, record("b", &["a"], "y", JobStatus::Pending)];
        p
    }

    fn assigned(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ToRunner>) -> Box<Assignment> {
        match rx.try_recv() {
            Ok(ToRunner::Assign { assignment }) => assignment,
            other => panic!("expected an assignment, got {other:?}"),
        }
    }

    /// `job` in a container of `image`, tagged `tag`, on its runner.
    fn in_image(mut job: JobRecord, image: &str, tag: &str) -> JobRecord {
        job.place = Some(Place::Container {
            image: image.to_owned(),
            tag: tag.to_owned(),
        });
        job
    }

    /// Pipeline `number` of `view` with these jobs.
    fn numbered(number: u64, jobs: Vec<JobRecord>) -> Pipeline {
        let mut p = pipeline("dev");
        p.number = number;
        p.repo = "view".to_owned();
        p.sha = Some("abc".to_owned());
        p.jobs = jobs;
        p
    }

    #[test]
    fn jobs_of_one_pipeline_at_one_place_take_the_workspace_in_turn() {
        use JobStatus::{Pending, Succeeded};
        let state = scratch("place-turns");
        let mut s = scheduler(&state);
        let mut x = connect_offering(&s.runners, "x", 0, true);
        // Two tags of one image, as a changed Containerfile makes: the tag is
        // no part of the place, so the jobs share a workspace.
        s.running.push(numbered(
            1,
            vec![
                in_image(
                    record("a", &[], "x", Pending),
                    "x",
                    "localhost/zci-view-x:1",
                ),
                in_image(
                    record("b", &[], "x", Pending),
                    "x",
                    "localhost/zci-view-x:2",
                ),
            ],
        ));

        s.assign(0, 1);
        let b = &s.running[0].jobs[1];
        assert_eq!(
            (b.status, b.note.as_str()),
            (JobStatus::Waiting, "waiting for a in workspace image-x")
        );
        assert!(x.try_recv().is_err());

        s.assign(0, 0);
        assert_eq!(assigned(&mut x).run, "view.1.a");
        s.assign(0, 1);
        assert_eq!(
            s.running[0].jobs[1].note,
            "waiting for a in workspace image-x"
        );
        assert!(x.try_recv().is_err());

        s.running[0].jobs[0].status = Succeeded;
        s.assign(0, 1);
        assert_eq!(assigned(&mut x).run, "view.1.b");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_newer_pipeline_waits_for_the_older_ones_job_at_its_place() {
        use JobStatus::{Pending, Succeeded};
        let state = scratch("place-queue");
        let mut s = scheduler(&state);
        let mut x = connect_offering(&s.runners, "x", 0, true);
        let a = |status| in_image(record("a", &[], "x", status), "x", "localhost/zci-view-x:1");
        s.running.push(numbered(1, vec![a(Pending)]));
        s.running.push(numbered(2, vec![a(Pending)]));

        // #1's job has not started, and it is older: #2's queues behind it.
        s.assign(1, 0);
        assert_eq!(
            (
                s.running[1].jobs[0].status,
                s.running[1].jobs[0].note.as_str()
            ),
            (JobStatus::Waiting, "workspace image-x queued for #1")
        );
        assert!(x.try_recv().is_err());

        // The older job is never held back by the younger one.
        s.assign(0, 0);
        assert_eq!(assigned(&mut x).run, "view.1.a");
        s.assign(1, 0);
        assert_eq!(s.running[1].jobs[0].note, "workspace image-x in use by #1");
        assert!(x.try_recv().is_err());

        s.running[0].jobs[0].status = Succeeded;
        s.assign(1, 0);
        assert_eq!(assigned(&mut x).run, "view.2.a");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn the_same_image_on_two_runners_or_in_two_repositories_is_no_shared_workspace() {
        use JobStatus::{Pending, Running};
        let state = scratch("place-apart");
        let mut s = scheduler(&state);
        let _x = connect_offering(&s.runners, "x", 0, true);
        let mut y = connect_offering(&s.runners, "y", 0, true);
        let at = |runner: &str, status| in_image(record("a", &[], runner, status), "x", "t");
        // The same image at runner x, and in another repository at runner y.
        s.running.push(numbered(1, vec![at("x", Running)]));
        let mut elsewhere = numbered(1, vec![at("y", Running)]);
        elsewhere.repo = "other".to_owned();
        s.running.push(elsewhere);
        s.running.push(numbered(2, vec![at("y", Pending)]));

        s.assign(2, 0);
        assert_eq!(assigned(&mut y).run, "view.2.a");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn the_checkout_wipes_only_where_a_clean_pipeline_first_uses_a_place() {
        use JobStatus::{Pending, Skipped, Succeeded};
        let mut p = numbered(
            1,
            vec![
                record("a", &[], "x", Succeeded),
                record("b", &[], "x", Pending),
            ],
        );
        p.jobs[0].started = Some(1);
        // Without a clean channel the ignored files stay.
        assert_eq!(checkout_of(&p, 0), Checkout::Clean);
        assert_eq!(checkout_of(&p, 1), Checkout::Clean);

        p.clean = true;
        assert_eq!(checkout_of(&p, 0), Checkout::Wipe);
        assert_eq!(checkout_of(&p, 1), Checkout::Clean);

        // Nothing started there before it, or somewhere else: nothing to keep.
        p.jobs[0].status = Skipped;
        p.jobs[0].started = None;
        assert_eq!(checkout_of(&p, 1), Checkout::Wipe);
        p.jobs[0].started = Some(1);
        p.jobs[0].runner = Some("y".to_owned());
        assert_eq!(checkout_of(&p, 1), Checkout::Wipe);
        p.jobs[0].runner = Some("x".to_owned());
        p.jobs[0].place = Some(Place::Machine {
            name: "m".to_owned(),
        });
        assert_eq!(checkout_of(&p, 1), Checkout::Wipe);
    }

    #[test]
    fn a_clean_pipeline_wipes_each_place_once() {
        use JobStatus::{Pending, Succeeded};
        let state = scratch("place-clean");
        let mut s = scheduler(&state);
        let mut x = connect_offering(&s.runners, "x", 0, true);
        let mut p = numbered(
            5,
            vec![
                record("a", &[], "x", Pending),
                record("b", &[], "x", Pending),
                in_image(record("c", &[], "x", Pending), "x", "t"),
            ],
        );
        p.clean = true;
        s.running.push(p);

        s.assign(0, 0);
        assert_eq!(assigned(&mut x).checkout, Checkout::Wipe);
        s.running[0].jobs[0].status = Succeeded;
        s.assign(0, 1);
        assert_eq!(assigned(&mut x).checkout, Checkout::Clean);
        // Another place is another workspace, which the pipeline has not used.
        s.assign(0, 2);
        assert_eq!(assigned(&mut x).checkout, Checkout::Wipe);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A repository with a clean `nightly` channel and two jobs whose headers
    /// still carry a `context`, read the way a pipeline is set up.
    #[test]
    fn setting_up_marks_a_clean_pipeline_and_runs_jobs_that_carry_a_context() {
        let origin = scratch("setup-origin");
        let state = scratch("setup-state");
        let author = [
            ("GIT_AUTHOR_NAME".to_owned(), "t".to_owned()),
            ("GIT_AUTHOR_EMAIL".to_owned(), "t@t".to_owned()),
            ("GIT_COMMITTER_NAME".to_owned(), "t".to_owned()),
            ("GIT_COMMITTER_EMAIL".to_owned(), "t@t".to_owned()),
        ];
        let ci = origin.join(CI_DIR);
        std::fs::create_dir_all(&ci).unwrap();
        for name in ["a", "b"] {
            std::fs::write(
                ci.join(format!("{name}.sh")),
                "# /// ci\n# on = [\"push *\", \"cron 0 3 * * *\"]\n# context = \"box\"\n# ///\necho\n",
            )
            .unwrap();
        }
        std::fs::write(
            ci.join("channels.toml"),
            "[[channel]]\nname = \"nightly\"\non = [\"cron *\"]\nclean = true\n",
        )
        .unwrap();
        git(&origin, &["init", "-q", "-b", "main"], &author).unwrap();
        git(&origin, &["add", "."], &author).unwrap();
        git(&origin, &["commit", "-q", "-m", "ci"], &author).unwrap();

        let config = Arc::new(
            CiConfig::parse(&format!(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"{FP_1}\"\n\
                 [repos.view]\nurl = \"{}\"\n",
                origin.display()
            ))
            .expect("a valid config"),
        );
        let mut s = scheduler_over(&state, Arc::clone(&config));
        let repo = config.repos["view"].clone();
        for (kind, cron, channel, clean) in [
            (EventKind::Push, None, "main", false),
            (EventKind::Cron, Some("0 3 * * *"), "nightly", true),
        ] {
            let mut p = pipeline("");
            p.event.kind = kind;
            p.event.cron = cron.map(str::to_owned);
            let event = p.event.clone();
            let planned = s
                .set_up("view", &repo, &event, &mut p)
                .expect("set up")
                .expect("a CI folder");
            assert_eq!((p.channel.as_str(), p.clean), (channel, clean));
            let jobs: Vec<(&str, &[&str])> = planned
                .iter()
                .map(|j| (j.def.name.as_str(), j.def.ignored_keys.as_slice()))
                .collect();
            assert_eq!(jobs, [("a", &["context"][..]), ("b", &["context"][..])]);
        }
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&state);
    }

    // ---- manual runs

    /// The `.zeughaus-ci/` of a snapshot: `build` and `check` after it, a job
    /// nobody asks for, and one on a VM.
    const SNAPSHOT: &[(&str, &str)] = &[
        (
            ".zeughaus-ci/build.sh",
            "# /// ci\n# on = [\"push main\"]\n# machine = \"atik\"\n# secrets = [\"TOKEN\"]\n# ///\necho build\n",
        ),
        (
            ".zeughaus-ci/check.sh",
            "# /// ci\n# needs = [\"build\"]\n# machine = \"atik\"\n# ///\necho check\n",
        ),
        (
            ".zeughaus-ci/other.sh",
            "# /// ci\n# on = [\"push main\"]\n# machine = \"atik\"\n# ///\necho other\n",
        ),
        (
            ".zeughaus-ci/windows.ps1",
            "# /// ci\n# on = [\"push main\"]\n# platform = \"windows-amd64\"\n# ///\nWrite-Output windows\n",
        ),
        (
            ".zeughaus-ci/channels.toml",
            "[[channel]]\nname = \"dev\"\non = [\"push main\"]\n",
        ),
    ];

    /// An upload the way the `/manual` service leaves it, and the event that
    /// names it. Every call is an upload of its own.
    fn stored_manual(state: &Path, repo: &str, jobs: &[&str], sha: &str) -> CiEvent {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = format!("{:016x}", NEXT.fetch_add(1, Ordering::Relaxed));
        let upload = RunUpload {
            origin: format!("git@example.org:me/{repo}.git"),
            actor: "hannes@tuco-pc".to_owned(),
            summary: "fix(core): thing\nsecond line".to_owned(),
            jobs: jobs.iter().map(|job| (*job).to_owned()).collect(),
            dirs: vec![
                UploadDir {
                    dir: repo.to_owned(),
                    sha: sha.to_owned(),
                },
                UploadDir {
                    dir: "lib".to_owned(),
                    sha: "b".repeat(40),
                },
            ],
            sizes: vec![1, 1],
        };
        std::fs::create_dir_all(manual::upload_dir(state, &id)).unwrap();
        manual::write_upload(state, &id, &upload).unwrap();
        CiEvent {
            repo: repo.to_owned(),
            kind: EventKind::Manual,
            git_ref: "main".to_owned(),
            sha: Some(sha.to_owned()),
            cron: None,
            actor: upload.actor,
            delivery: None,
            upload: Some(id),
            received: 0,
        }
    }

    /// One commit that holds `files`, in a repository of its own, bundled
    /// the way a snapshot is: one ref under `refs/zci-manual/`. Returns the
    /// bundle and the commit.
    fn bundle_of(dir: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
        let author = [
            ("GIT_AUTHOR_NAME".to_owned(), "t".to_owned()),
            ("GIT_AUTHOR_EMAIL".to_owned(), "t@t".to_owned()),
            ("GIT_COMMITTER_NAME".to_owned(), "t".to_owned()),
            ("GIT_COMMITTER_EMAIL".to_owned(), "t@t".to_owned()),
        ];
        let src = dir.join("src");
        for (name, text) in files {
            let path = src.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        git(&src, &["init", "-q", "-b", "main"], &author).unwrap();
        git(&src, &["add", "-A"], &author).unwrap();
        git(&src, &["commit", "-q", "-m", "zeughaus manual"], &author).unwrap();
        git(
            &src,
            &["update-ref", "refs/zci-manual/1-1", "HEAD"],
            &author,
        )
        .unwrap();
        let bundle = dir.join("0.bundle");
        let to = bundle.to_string_lossy();
        git(
            &src,
            &["bundle", "create", &to, "refs/zci-manual/1-1"],
            &author,
        )
        .unwrap();
        let sha = git(&src, &["rev-parse", "HEAD"], &[]).unwrap();
        (bundle, sha.trim().to_owned())
    }

    /// Puts `bundle` where the service left the first bundle of the upload
    /// `event` names.
    fn with_bundle(state: &Path, event: &CiEvent, bundle: &Path) {
        let id = event.upload.as_deref().unwrap();
        std::fs::copy(bundle, manual::bundle_path(state, id, 0)).unwrap();
    }

    #[test]
    fn a_manual_run_is_the_named_jobs_of_the_snapshots_pipeline() {
        let state = scratch("manual-jobs");
        let (bundle, sha) = bundle_of(&scratch("manual-jobs-src"), SNAPSHOT);
        let s = scheduler_over(&state, Arc::new(config()));
        let set_up = |jobs: &[&str]| {
            let event = stored_manual(&state, "demo", jobs, &sha);
            with_bundle(&state, &event, &bundle);
            let mut p = pipeline("");
            let planned = s.set_up_manual(&event, &mut p);
            (p, planned)
        };

        // In the order of the folder's jobs, whatever the order asked for,
        // each where its header places it.
        let (p, planned) = set_up(&["check", "build"]);
        let planned = planned.expect("set up").expect("jobs");
        let names: Vec<&str> = planned.iter().map(|j| j.def.name.as_str()).collect();
        assert_eq!(names, ["build", "check"]);
        assert!(
            planned
                .iter()
                .all(|j| j.runner == "atik" && j.place == Place::Host)
        );
        assert_eq!(planned[1].def.needs, ["build"]);
        // The code of a working tree gets no secrets.
        assert!(planned[0].def.secrets.is_empty());
        assert_eq!(p.sha.as_deref(), Some(sha.as_str()));
        assert_eq!(
            (p.subject.as_str(), p.channel.as_str(), p.clean),
            ("fix(core): thing (check, build)", "dev", false)
        );

        // A need on a job the run does not name is dropped, and a job it
        // does not name is not run.
        let (_, planned) = set_up(&["check"]);
        let planned = planned.expect("set up").expect("jobs");
        assert_eq!(planned.len(), 1);
        assert!(planned[0].def.needs.is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_manual_run_that_cannot_be_set_up_says_why() {
        let state = scratch("manual-refused");
        let (bundle, sha) = bundle_of(&scratch("manual-refused-src"), SNAPSHOT);
        let (bare, bare_sha) = bundle_of(&scratch("manual-refused-bare"), &[("README", "no ci\n")]);
        let s = scheduler_over(&state, Arc::new(config()));
        let refused = |event: &CiEvent| {
            s.set_up_manual(event, &mut pipeline(""))
                .err()
                .expect("a pipeline that does not start")
        };
        let uploaded = |repo: &str, jobs: &[&str], bundle: &Path, sha: &str| {
            let event = stored_manual(&state, repo, jobs, sha);
            with_bundle(&state, &event, bundle);
            event
        };

        assert_eq!(
            refused(&uploaded("demo", &["zzz"], &bundle, &sha)),
            "no job zzz in .zeughaus-ci/ of the working tree; its jobs are build, check, other, windows"
        );
        assert_eq!(
            refused(&uploaded("demo", &["windows"], &bundle, &sha)),
            "manual runs run on a runner's host or in a container; windows runs on the machine win11"
        );
        assert_eq!(
            refused(&uploaded("demo", &["build"], &bare, &bare_sha)),
            "no .zeughaus-ci/ in the working tree"
        );
        assert_eq!(
            refused(&uploaded("nowhere", &["build"], &bundle, &sha)),
            "nowhere is no repository of ci.toml"
        );

        // An event with no upload, one whose upload is gone, and one whose
        // id is no id.
        let mut event = stored_manual(&state, "demo", &["build"], &sha);
        let id = event.upload.take();
        assert_eq!(refused(&event), "a manual event carries no upload");
        event.upload = Some("fedcba9876543210".to_owned());
        assert_eq!(refused(&event), "manual upload fedcba9876543210 is gone");
        event.upload = id.map(|id| format!("../{id}"));
        assert_eq!(refused(&event), "a manual event carries no upload");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The image of a manual job is read from the snapshot's own
    /// Containerfile, which only its bundle has; the mirror it is read from
    /// goes with the last manual run of the repository.
    #[test]
    fn a_manual_job_with_an_image_reads_its_containerfile_from_the_snapshot() {
        let state = scratch("manual-image");
        let (bundle, sha) = bundle_of(
            &scratch("manual-image-src"),
            &[
                (".zeughaus-ci/trixie.Containerfile", "FROM debian:trixie\n"),
                (
                    ".zeughaus-ci/arm.sh",
                    "# /// ci\n# on = [\"push main\"]\n# platform = \"linux-arm64\"\n# image = \"trixie\"\n# ///\necho arm\n",
                ),
                (
                    ".zeughaus-ci/vm.sh",
                    "# /// ci\n# on = [\"push main\"]\n# platform = \"windows-amd64\"\n# image = \"trixie\"\n# ///\necho vm\n",
                ),
            ],
        );
        let mut s = scheduler_over(&state, Arc::new(config()));
        let event = stored_manual(&state, "demo", &["arm"], &sha);
        let id = event.upload.clone().unwrap();
        with_bundle(&state, &event, &bundle);

        let mut p = pipeline("");
        p.repo = "demo".to_owned();
        p.event = event.clone();
        let planned = s.set_up_manual(&event, &mut p).unwrap().unwrap();
        let digest = hex::encode(Sha256::digest(b"FROM debian:trixie\n"));
        assert_eq!(planned[0].runner, "atik");
        assert_eq!(
            planned[0].place,
            Place::Container {
                image: "trixie".to_owned(),
                tag: format!("localhost/zci-demo-trixie:{}", &digest[..16]),
            }
        );
        let mirror = manual::mirror_dir(&state, "demo");
        assert!(mirror.join("HEAD").exists());

        // A VM runs no container.
        let on_vm = stored_manual(&state, "demo", &["vm"], &sha);
        with_bundle(&state, &on_vm, &bundle);
        let err = s.set_up_manual(&on_vm, &mut pipeline("")).err().unwrap();
        assert!(err.contains("an image needs a runner"), "{err}");

        // Another manual run of the repository is still running: the mirror
        // stays.
        let mut other = pipeline("dev");
        other.repo = "demo".to_owned();
        other.event.kind = EventKind::Manual;
        s.running.push(other);
        s.drop_manual(&p);
        assert!(!manual::upload_dir(&state, &id).exists());
        assert!(mirror.exists());
        s.running.clear();
        s.drop_manual(&p);
        assert!(!mirror.exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_manual_run_that_cannot_start_ends_as_an_error_pipeline_its_uploader_hears_of() {
        let state = scratch("manual-error");
        let (bundle, sha) = bundle_of(&scratch("manual-error-src"), SNAPSHOT);
        let mut s = scheduler_over(&state, Arc::new(config()));
        let event = stored_manual(&state, "demo", &["windows"], &sha);
        let id = event.upload.clone().unwrap();
        with_bundle(&state, &event, &bundle);
        let key = event::queue_key(&event);
        s.start_pipeline(Path::new("/nonexistent/queue.json"), event, key);

        assert!(s.running.is_empty());
        let record = load_pipeline(&state, "demo", 1).expect("the record");
        assert_eq!(record.status, PipelineStatus::Error);
        assert_eq!(
            record.note,
            "manual runs run on a runner's host or in a container; windows runs on the machine win11"
        );
        assert_eq!(
            (record.channel(), record.event.kind),
            ("dev".to_owned(), EventKind::Manual)
        );
        assert!(record.jobs.is_empty() && record.finished.is_some());
        // The service learns the number from a file beside the upload, which
        // is gone: nothing runs.
        let number = std::fs::read_to_string(manual::number_path(&state, &id));
        assert_eq!(number.as_deref().ok(), Some("1"));
        assert!(!manual::upload_dir(&state, &id).exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// From the upload to the assignment to the cleaning up, with a second
    /// manual run that waits for the workspace the first one holds.
    #[test]
    fn a_manual_run_is_assigned_with_its_working_trees_and_cleans_up_after_itself() {
        let state = scratch("manual-run");
        let (bundle, sha) = bundle_of(
            &scratch("manual-run-src"),
            &[(
                ".zeughaus-ci/a.sh",
                "# /// ci\n# on = [\"push main\"]\n# ///\necho a\n",
            )],
        );
        let mut s = scheduler(&state);
        let mut x = connect(&s.runners, "x", 0);
        let start = |s: &mut Scheduler| {
            let event = stored_manual(&state, "view", &["a"], &sha);
            let id = event.upload.clone().unwrap();
            with_bundle(&state, &event, &bundle);
            let key = event::queue_key(&event);
            s.start_pipeline(Path::new("/nonexistent/queue.json"), event, key);
            id
        };
        let first = start(&mut s);
        let second = start(&mut s);
        assert_eq!(s.running.len(), 2);
        let said = |id: &str| std::fs::read_to_string(manual::number_path(&state, id)).ok();
        assert_eq!(said(&first).as_deref(), Some("1"));
        assert_eq!(said(&second).as_deref(), Some("2"));

        s.advance();
        let a = assigned(&mut x);
        assert_eq!(
            (a.run.as_str(), a.job.as_str(), a.number),
            ("view.1.a", "a", 1)
        );
        assert_eq!(a.checkout, Checkout::Clean);
        // No forge, no secrets: the working trees come in bundles.
        assert_eq!(
            (
                a.url.as_str(),
                a.fetch_user.as_str(),
                a.fetch_token.as_deref()
            ),
            ("", "", None)
        );
        assert!(a.secrets.is_empty() && a.inputs.is_empty());
        assert_eq!(
            (a.file.as_str(), a.interpreter.as_str(), &a.place),
            ("a.sh", "sh", &Place::Host)
        );
        assert_eq!(a.sha, sha);
        let dirs: Vec<(&str, &str)> = a
            .upload
            .iter()
            .flat_map(|upload| &upload.dirs)
            .map(|d| (d.dir.as_str(), d.sha.as_str()))
            .collect();
        assert_eq!(
            dirs,
            [("view", sha.as_str()), ("lib", "b".repeat(40).as_str())]
        );
        let env = |name: &str| {
            a.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(
            (env("CI_EVENT"), env("CI_REF"), env("CI_REPO")),
            (Some("manual"), Some("main"), Some("view"))
        );
        assert_eq!(env("CI_ACTOR"), Some("hannes@tuco-pc"));

        // The second run waits for the workspace the first holds at its
        // place, which every pipeline and manual run of the repository
        // shares on this runner.
        let waiting = &s.running[1].jobs[0];
        assert_eq!(
            (waiting.status, waiting.note.as_str()),
            (JobStatus::Waiting, "workspace host in use by #1")
        );
        assert!(x.try_recv().is_err());

        let message = |message| Msg::FromRunner {
            name: "x".to_owned(),
            message,
        };
        s.handle(message(FromRunner::Started {
            run: "view.1.a".to_owned(),
        }));
        s.handle(message(FromRunner::Finished {
            run: "view.1.a".to_owned(),
            end: RunEnd {
                code: Some(0),
                killed: false,
                timed_out: false,
                artifacts: false,
            },
        }));
        s.finish_pipelines();
        assert_eq!(s.running.len(), 1);
        assert_eq!(
            load_pipeline(&state, "view", 1).unwrap().status,
            PipelineStatus::Succeeded
        );
        assert!(!manual::upload_dir(&state, &first).exists());
        assert!(manual::upload_dir(&state, &second).exists());

        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::Forget {
                run: "view.1.a".to_owned()
            })
        );
        s.advance();
        assert_eq!(assigned(&mut x).run, "view.2.a");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_pipeline_record_from_before_clean_channels_still_loads() {
        let mut p = pipeline("nightly");
        p.clean = true;
        let json = serde_json::to_value(&p).unwrap();
        assert!(
            serde_json::from_value::<Pipeline>(json.clone())
                .unwrap()
                .clean
        );
        let mut old = json;
        assert!(old.as_object_mut().unwrap().remove("clean").is_some());
        assert!(!serde_json::from_value::<Pipeline>(old).unwrap().clean);
    }

    #[test]
    fn a_job_record_from_before_transfers_still_loads() {
        let mut json = serde_json::to_value(record("a", &[], "x", JobStatus::Pending)).unwrap();
        let job = json.as_object_mut().unwrap();
        assert!(job.remove("assigned").is_some());
        assert!(job.remove("transfers").is_some());
        let old = serde_json::from_value::<JobRecord>(json).unwrap();
        assert_eq!(old.assigned, None);
        assert!(old.transfers.is_empty());
    }

    #[test]
    fn transfer_reports_land_in_the_consumers_record() {
        let state = scratch("transfers");
        let mut s = scheduler(&state);
        // `b`, the consumer, runs on `y`.
        s.running.push(two_jobs("x"));
        let report = |runner: &str, need: &str, event| Msg::FromRunner {
            name: runner.to_owned(),
            message: FromRunner::Transfer {
                run: "view.7.b".to_owned(),
                need: need.to_owned(),
                event,
            },
        };
        let transfers = |s: &Scheduler| s.running[0].jobs[1].transfers.clone();

        // Only the consumer's runner reports on it, and only about its needs.
        s.handle(report("x", "a", TransferEvent::Started));
        s.handle(report("y", "zzz", TransferEvent::Started));
        assert!(transfers(&s).is_empty());

        s.handle(report("y", "a", TransferEvent::Started));
        let open = transfers(&s);
        assert_eq!(open.len(), 1);
        assert_eq!((open[0].need.as_str(), open[0].finished), ("a", None));
        s.handle(report("x", "a", TransferEvent::Done { bytes: 99 }));
        assert_eq!(transfers(&s), open, "x is not the runner of b");

        s.handle(report("y", "a", TransferEvent::Done { bytes: 10 }));
        let done = transfers(&s);
        assert_eq!(done.len(), 1);
        assert_eq!((done[0].bytes, done[0].error.as_deref()), (Some(10), None));
        assert!(done[0].finished.is_some());

        // A new attempt replaces one that never ended; a failure keeps its
        // message.
        s.handle(report("y", "a", TransferEvent::Started));
        s.handle(report("y", "a", TransferEvent::Started));
        let failure = TransferEvent::Failed {
            message: "the CI is gone".to_owned(),
        };
        s.handle(report("y", "a", failure));
        let all = transfers(&s);
        assert_eq!(all.len(), 2, "the first stays, the two starts are one try");
        assert_eq!(all[0], done[0]);
        assert_eq!(
            (all[1].bytes, all[1].error.as_deref()),
            (None, Some("the CI is gone"))
        );
        assert!(all[1].finished.is_some());

        // The record on disk is the one the editors are told about.
        let saved = load_pipeline(&state, "view", 7).unwrap();
        assert_eq!(saved.jobs[1].transfers, all);

        // A job that ended takes no more reports.
        s.running[0].jobs[1].status = JobStatus::Failed;
        s.handle(report("y", "a", TransferEvent::Started));
        assert_eq!(transfers(&s), all);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_started_job_has_no_transfer_left_open() {
        let state = scratch("started-transfers");
        let mut s = scheduler(&state);
        let mut p = two_jobs("x");
        p.jobs[1].status = JobStatus::Starting;
        p.jobs[1].assigned = Some(5);
        // The report of this transfer's end never arrived.
        p.jobs[1].transfers.push(TransferRecord {
            need: "a".to_owned(),
            started: 6,
            finished: None,
            bytes: None,
            error: None,
        });
        s.running.push(p);
        s.runners.assign("view.7.b", "y");
        s.handle(Msg::FromRunner {
            name: "y".to_owned(),
            message: FromRunner::Started {
                run: "view.7.b".to_owned(),
            },
        });
        let b = &s.running[0].jobs[1];
        assert_eq!(b.status, JobStatus::Running);
        assert!(b.transfers[0].finished.is_some());
        let _ = std::fs::remove_dir_all(&state);
    }

    fn finished(runner: &str, run: &str, artifacts: bool) -> Msg {
        Msg::FromRunner {
            name: runner.to_owned(),
            message: FromRunner::Finished {
                run: run.to_owned(),
                end: RunEnd {
                    code: Some(0),
                    killed: false,
                    timed_out: false,
                    artifacts,
                },
            },
        }
    }

    #[test]
    fn a_succeeded_need_is_fetched_early_by_the_runner_of_a_pending_consumer() {
        let state = scratch("prefetch");
        let mut s = scheduler(&state);
        let mut x = connect(&s.runners, "x", 0);
        let mut y = connect(&s.runners, "y", 0);
        // `a` runs on x. `b` and `g` wait for it on y; the rest must not be
        // told to fetch: `c` runs on x, where the set is linked, `d` was
        // handed its inputs already, `e` needs nothing and `f`'s runner is
        // not connected.
        let mut p = two_jobs("x");
        p.jobs[0].status = JobStatus::Running;
        p.jobs[0].artifacts = false;
        p.jobs.extend([
            record("c", &["a"], "x", JobStatus::Pending),
            record("d", &["a"], "y", JobStatus::Starting),
            record("e", &[], "y", JobStatus::Pending),
            record("f", &["a"], "z", JobStatus::Pending),
            record("g", &["a"], "y", JobStatus::Waiting),
        ]);
        s.running.push(p);
        s.runners.assign("view.7.a", "x");

        s.handle(finished("x", "view.7.a", true));
        let early = |consumer: &str| ToRunner::Prefetch {
            run: "view.7.a".to_owned(),
            consumer: consumer.to_owned(),
            need: "a".to_owned(),
        };
        assert_eq!(y.try_recv().ok(), Some(early("view.7.b")));
        assert_eq!(y.try_recv().ok(), Some(early("view.7.g")));
        assert!(y.try_recv().is_err());
        // The producer's runner is only told the CI is done with the run.
        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::Forget {
                run: "view.7.a".to_owned()
            })
        );
        assert!(x.try_recv().is_err());

        // A run that left no set has nothing to fetch.
        let mut none = two_jobs("x");
        none.number = 8;
        none.jobs[0].status = JobStatus::Running;
        none.jobs[0].artifacts = false;
        s.running.push(none);
        s.runners.assign("view.8.a", "x");
        s.handle(finished("x", "view.8.a", false));
        assert!(y.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn what_was_fetched_early_for_a_job_that_never_ran_is_dropped() {
        let state = scratch("drop-prefetch");
        let mut s = scheduler(&state);
        let mut y = connect(&s.runners, "y", 0);
        let fetched = TransferRecord {
            need: "a".to_owned(),
            started: 1,
            finished: Some(2),
            bytes: Some(3),
            error: None,
        };
        // `a` left a set and `b` took it early; `c` failed, so `b` is skipped.
        // `d` waits for `c` alone and fetched nothing.
        let mut p = two_jobs("x");
        p.jobs[0].artifacts = true;
        p.jobs.push(record("c", &[], "x", JobStatus::Failed));
        p.jobs[1].def.needs.push("c".to_owned());
        p.jobs[1].transfers.push(fetched.clone());
        p.jobs.push(record("d", &["c"], "y", JobStatus::Pending));
        s.running.push(p);
        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Skipped);
        assert_eq!(s.running[0].jobs[3].status, JobStatus::Skipped);
        assert_eq!(
            y.try_recv().ok(),
            Some(ToRunner::DropPrefetch {
                consumer: "view.7.b".to_owned()
            })
        );
        assert!(y.try_recv().is_err(), "`d` fetched nothing");

        // A cancelled pipeline skips its pending jobs the same way.
        let mut p = two_jobs("x");
        p.number = 8;
        p.jobs[1].transfers.push(fetched.clone());
        s.running.push(p);
        s.running[1].cancelled = true;
        s.apply_cancel(1);
        assert_eq!(
            y.try_recv().ok(),
            Some(ToRunner::DropPrefetch {
                consumer: "view.8.b".to_owned()
            })
        );

        // So does a job that fails before it is handed over, and one whose
        // runner stayed away; a job that was handed over keeps what it has.
        let mut p = two_jobs("x");
        p.number = 9;
        p.jobs[1].transfers.push(fetched.clone());
        s.running.push(p);
        s.fail_job(2, 1, "secret g is not granted".to_owned(), State::Error);
        assert_eq!(
            y.try_recv().ok(),
            Some(ToRunner::DropPrefetch {
                consumer: "view.9.b".to_owned()
            })
        );
        let mut p = two_jobs("x");
        p.number = 10;
        p.jobs[1].transfers.push(fetched.clone());
        s.running.push(p);
        s.skip_offline(3, 1, "y");
        assert_eq!(
            y.try_recv().ok(),
            Some(ToRunner::DropPrefetch {
                consumer: "view.10.b".to_owned()
            })
        );
        let mut p = two_jobs("x");
        p.number = 11;
        p.jobs[1].status = JobStatus::Starting;
        p.jobs[1].assigned = Some(5);
        p.jobs[1].transfers.push(fetched);
        s.running.push(p);
        s.fail_job(4, 1, "cannot fetch inputs of a".to_owned(), State::Error);
        assert!(y.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn an_input_from_another_runner_waits_for_that_runner() {
        let state = scratch("remote-input");
        let mut s = scheduler(&state);
        let mut y = connect(&s.runners, "y", 0);
        s.running.push(two_jobs("x"));

        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Waiting);
        assert_eq!(
            s.running[0].jobs[1].note,
            "waiting for runner x (artifacts of a)"
        );
        assert!(y.try_recv().is_err());

        let _x = connect(&s.runners, "x", 0);
        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Starting);
        assert_eq!(
            s.running[0].jobs[1].assigned, s.running[0].jobs[1].started,
            "assigned is stamped with the handover"
        );
        assert!(s.running[0].jobs[1].assigned.is_some());
        let assignment = assigned(&mut y);
        assert_eq!(assignment.run, "view.7.b");
        assert_eq!(
            assignment.inputs,
            [Input {
                need: "a".to_owned(),
                source: InputSource::Ci {
                    run: "view.7.a".to_owned()
                }
            }]
        );
        assert_eq!(s.runners.assigned_to("view.7.b").as_deref(), Some("y"));
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn an_input_from_the_same_runner_is_local() {
        let state = scratch("local-input");
        let mut s = scheduler(&state);
        let mut y = connect(&s.runners, "y", 0);
        s.running.push(two_jobs("y"));
        s.advance();
        let assignment = assigned(&mut y);
        assert_eq!(
            assignment.inputs,
            [Input {
                need: "a".to_owned(),
                source: InputSource::Local {
                    run: "view.7.a".to_owned()
                }
            }]
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_job_skips_when_its_runner_stays_offline() {
        let state = scratch("offline");
        let mut s = scheduler(&state);
        s.running.push(two_jobs("y"));
        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Waiting);
        assert_eq!(s.running[0].jobs[1].note, "waiting for runner y");
        // `wait_minutes` of `y` is one.
        s.ready_since
            .insert(("view".to_owned(), 7, "b".to_owned()), 0);
        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Skipped);
        assert_eq!(s.running[0].jobs[1].note, "runner y offline");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_hello_requeues_a_handed_over_job_the_runner_does_not_know() {
        let state = scratch("hello");
        let mut s = scheduler(&state);
        let _y = connect(&s.runners, "y", 100);
        let mut p = two_jobs("y");
        p.jobs[1].status = JobStatus::Starting;
        p.jobs[1].started = Some(50);
        p.jobs[1].assigned = Some(50);
        p.jobs[1].transfers.push(TransferRecord {
            need: "a".to_owned(),
            started: 51,
            finished: None,
            bytes: None,
            error: None,
        });
        s.running.push(p);
        s.runners.assign("view.7.b", "y");
        s.handle(Msg::RunnerHello {
            name: "y".to_owned(),
            runs: Vec::new(),
            artifacts: Vec::new(),
        });
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Pending);
        assert_eq!(s.running[0].jobs[1].assigned, None);
        assert!(s.running[0].jobs[1].transfers.is_empty());
        assert_eq!(s.runners.assigned_to("view.7.b"), None);

        // Handed over after the connection began: the runner has it queued.
        s.running[0].jobs[1].status = JobStatus::Starting;
        s.running[0].jobs[1].started = Some(150);
        s.handle(Msg::RunnerHello {
            name: "y".to_owned(),
            runs: Vec::new(),
            artifacts: Vec::new(),
        });
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Starting);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_hello_stops_runs_nobody_waits_for() {
        let state = scratch("stray");
        let mut s = scheduler(&state);
        let mut y = connect(&s.runners, "y", 0);
        s.handle(Msg::RunnerHello {
            name: "y".to_owned(),
            runs: vec![RunReport {
                run: "view.9.zzz".to_owned(),
                end: None,
            }],
            artifacts: Vec::new(),
        });
        assert_eq!(
            y.try_recv().ok(),
            Some(ToRunner::Kill {
                run: "view.9.zzz".to_owned()
            })
        );
        assert_eq!(
            y.try_recv().ok(),
            Some(ToRunner::Forget {
                run: "view.9.zzz".to_owned()
            })
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_hello_drops_the_sets_no_pending_job_needs() {
        let state = scratch("held");
        let mut s = scheduler(&state);
        let mut x = connect(&s.runners, "x", 0);
        let mut p = two_jobs("x");
        p.jobs.push(record("c", &[], "x", JobStatus::Running));
        s.running.push(p);
        let mut done = two_jobs("x");
        done.number = 8;
        done.jobs[1].status = JobStatus::Succeeded;
        s.running.push(done);
        s.handle(Msg::RunnerHello {
            name: "x".to_owned(),
            runs: vec![RunReport {
                run: "view.7.c".to_owned(),
                end: None,
            }],
            artifacts: vec![
                // `b` still needs it.
                "view.7.a".to_owned(),
                // Not settled yet: the end may still be on its way.
                "view.7.c".to_owned(),
                // Its consumer ended.
                "view.8.a".to_owned(),
                // A pipeline this process no longer runs.
                "view.3.a".to_owned(),
            ],
        });
        let mut sent = Vec::new();
        while let Ok(message) = x.try_recv() {
            sent.push(message);
        }
        assert_eq!(
            sent,
            [
                ToRunner::DropArtifacts {
                    run: "view.8.a".to_owned()
                },
                ToRunner::DropArtifacts {
                    run: "view.3.a".to_owned()
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_finished_set_stays_until_its_last_consumer_ended() {
        let state = scratch("finished");
        let mut s = scheduler(&state);
        let mut x = connect(&s.runners, "x", 0);
        let mut p = two_jobs("x");
        p.jobs[0].status = JobStatus::Running;
        p.jobs[0].artifacts = false;
        s.running.push(p);
        s.runners.assign("view.7.a", "x");
        s.handle(Msg::FromRunner {
            name: "x".to_owned(),
            message: FromRunner::Finished {
                run: "view.7.a".to_owned(),
                end: RunEnd {
                    code: Some(0),
                    killed: false,
                    timed_out: false,
                    artifacts: true,
                },
            },
        });
        let a = &s.running[0].jobs[0];
        assert_eq!(
            (a.status, a.code, a.artifacts),
            (JobStatus::Succeeded, Some(0), true)
        );
        let exit = transcript_dir(&state, "view", 7, "a").join("exit");
        assert_eq!(std::fs::read_to_string(exit).unwrap(), "code=0\n");
        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::Forget {
                run: "view.7.a".to_owned()
            })
        );
        assert_eq!(s.runners.assigned_to("view.7.a"), None);

        // `b` has not run yet: the set stays.
        s.drop_artifacts();
        assert!(x.try_recv().is_err());

        // Failed counts as ended as much as succeeded does.
        s.running[0].jobs[1].status = JobStatus::Failed;
        s.drop_artifacts();
        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::DropArtifacts {
                run: "view.7.a".to_owned()
            })
        );
        assert!(!s.running[0].jobs[0].artifacts);
        s.drop_artifacts();
        assert!(x.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_set_whose_runner_is_offline_is_dropped_once_it_is_back() {
        let state = scratch("drop-offline");
        let mut s = scheduler(&state);
        let mut p = two_jobs("x");
        p.jobs[1].status = JobStatus::Skipped;
        s.running.push(p);
        s.drop_artifacts();
        assert!(s.running[0].jobs[0].artifacts);
        let mut x = connect(&s.runners, "x", 0);
        s.drop_artifacts();
        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::DropArtifacts {
                run: "view.7.a".to_owned()
            })
        );
        assert!(!s.running[0].jobs[0].artifacts);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_cancel_kills_the_active_job_and_skips_the_rest() {
        let state = scratch("cancel");
        let mut s = scheduler(&state);
        let mut x = connect(&s.runners, "x", 0);
        let mut p = pipeline("dev");
        p.sha = Some("abc".to_owned());
        p.jobs = vec![
            record("a", &[], "x", JobStatus::Running),
            record("b", &["a"], "x", JobStatus::Pending),
            record("c", &[], "x", JobStatus::Waiting),
            record("d", &[], "x", JobStatus::Succeeded),
        ];
        s.running.push(p);
        s.runners.assign("view.7.a", "x");

        // A request for a pipeline that is not running is dropped.
        event::write_cancel(&state, "view", 99).unwrap();
        event::write_cancel(&state, "view", 7).unwrap();
        s.cancel_requests();
        assert!(event::drain_cancels(&state).is_empty());

        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::Kill {
                run: "view.7.a".to_owned()
            })
        );
        assert!(x.try_recv().is_err());
        let status = |s: &Scheduler| {
            s.running[0]
                .jobs
                .iter()
                .map(|j| j.status)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            status(&s),
            [
                JobStatus::Running,
                JobStatus::Skipped,
                JobStatus::Skipped,
                JobStatus::Succeeded
            ]
        );
        assert_eq!(s.running[0].jobs[1].note, "cancelled");
        assert_eq!(s.running[0].jobs[2].note, "cancelled");
        // The flag is on disk before the runner answers.
        assert!(load_pipeline(&state, "view", 7).unwrap().cancelled);

        // A job put back to pending meanwhile does not start again.
        s.running[0].jobs[2].status = JobStatus::Pending;
        s.advance();
        assert_eq!(s.running[0].jobs[2].status, JobStatus::Skipped);
        assert!(x.try_recv().is_err());

        s.finish_pipelines();
        assert_eq!(s.running.len(), 1);

        s.handle(Msg::FromRunner {
            name: "x".to_owned(),
            message: FromRunner::Finished {
                run: "view.7.a".to_owned(),
                end: RunEnd {
                    code: None,
                    killed: true,
                    timed_out: false,
                    artifacts: false,
                },
            },
        });
        let a = &s.running[0].jobs[0];
        assert_eq!(
            (a.status, a.note.as_str()),
            (JobStatus::Skipped, "cancelled")
        );
        assert_eq!(a.excerpt, None);

        s.finish_pipelines();
        assert!(s.running.is_empty());
        let record = load_pipeline(&state, "view", 7).unwrap();
        assert_eq!(record.status, PipelineStatus::Cancelled);
        assert!(record.finished.is_some());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_cancelled_pipeline_is_finished_again_after_a_restart() {
        let state = scratch("cancel-recover");
        let mut s = scheduler(&state);
        let mut p = pipeline("dev");
        p.jobs = vec![
            record("a", &[], "x", JobStatus::Running),
            record("b", &["a"], "x", JobStatus::Pending),
        ];
        p.cancelled = true;
        p.save(&state, &s.changes);

        s.recover();
        assert_eq!(s.running.len(), 1);
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Skipped);
        assert_eq!(s.running[0].jobs[1].note, "cancelled");

        // The runner still has the run when it connects again: it is killed.
        let mut x = connect(&s.runners, "x", 0);
        s.handle(Msg::RunnerHello {
            name: "x".to_owned(),
            runs: vec![RunReport {
                run: "view.7.a".to_owned(),
                end: None,
            }],
            artifacts: Vec::new(),
        });
        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::Kill {
                run: "view.7.a".to_owned()
            })
        );
        let _ = std::fs::remove_dir_all(&state);
    }
}
