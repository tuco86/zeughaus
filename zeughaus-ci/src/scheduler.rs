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
    Assignment, FromRunner, Input, InputSource, Place, RunEnd, RunReport, ToRunner, WhenBusy,
    parse_run_id, run_id,
};

use crate::channels::{self, Channels};
use crate::config::{CiConfig, RepoConfig};
use crate::event::{self, CiEvent, EventKind};
use crate::forge::{self, State, Status};
use crate::header::JobDef;
use crate::runners::Runners;
use crate::secrets::Secrets;
use crate::streak::{self, Streak};
use crate::{CI_DIR, cleanup, excerpt, pipeline};

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
}

impl PipelineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PipelineStatus::Running => "running",
            PipelineStatus::Succeeded => "succeeded",
            PipelineStatus::Failed => "failed",
            PipelineStatus::Error => "error",
            PipelineStatus::NoJobs => "no-jobs",
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
    /// The runner the job is placed on, decided when the pipeline is set up.
    #[serde(default)]
    pub runner: Option<String>,
    /// Where on that runner it runs.
    #[serde(default)]
    pub place: Option<Place>,
    /// The run left artifacts, which its runner holds and uploads when a job
    /// on another runner needs them.
    #[serde(default)]
    pub artifacts: bool,
    /// The end of a failed run's log, before its debug shell (see
    /// [`excerpt`]).
    #[serde(default)]
    pub excerpt: Option<String>,
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
}

fn pipelines_dir(state_dir: &Path, repo: &str) -> PathBuf {
    crate::ci_dir(state_dir).join("pipelines").join(repo)
}

/// `<n>.artifacts`: the `<job>.tar` of every job whose artifacts a job on
/// another runner needs, as that job's runner uploaded them.
pub fn artifacts_dir(state_dir: &Path, repo: &str, number: u64) -> PathBuf {
    pipelines_dir(state_dir, repo).join(format!("{number}.artifacts"))
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
/// longer has is left alone, so a second run changes nothing.
fn backfill_subjects(state_dir: &Path, changes: &Sender<CiChange>) {
    let mut pipelines: Vec<Pipeline> = load_pipelines(state_dir)
        .into_iter()
        .filter(|p| p.subject.is_empty() && p.sha.is_some())
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
    /// A runner connected and reports the runs it knows.
    RunnerHello {
        name: String,
        runs: Vec<RunReport>,
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
            self.running.push(pipeline);
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
        self.advance();
        self.lost_runners();
        self.finish_pipelines();
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
            subject: String::new(),
            status: PipelineStatus::Running,
            note: String::new(),
            created: crate::now_secs(),
            finished: None,
            jobs: Vec::new(),
            channel: channels::fallback(&event),
        };
        let setup = self.set_up(&name, &repo, &event, &mut pipeline);
        let jobs = match setup {
            Ok(Some(jobs)) => jobs,
            Ok(None) => {
                self.no_ci_folder(pipeline, &repo);
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
                finished: None,
                runner: Some(planned.runner),
                place: Some(planned.place),
                artifacts: false,
                excerpt: None,
            })
            .collect();
        pipeline.save(&self.state_dir, &self.changes);
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
        // Only shown, so a commit git cannot describe leaves it empty.
        pipeline.subject = git(&mirror, &["log", "-1", "--format=%s", &sha], &[])
            .map(|subject| subject.trim().to_owned())
            .unwrap_or_default();
        let (files, containerfiles) = read_ci(&mirror, &sha)?;
        pipeline.channel = Channels::from_files(&files)?.of(event);
        // git has no empty directories: nothing listed is no folder.
        if files.is_empty() && containerfiles.is_empty() {
            return Ok(None);
        }
        let jobs = pipeline::load_jobs(&files, &containerfiles)?;
        let selected = pipeline::select(&jobs, event);
        let mut planned = Vec::new();
        for def in jobs.into_iter().filter(|job| selected.contains(&job.name)) {
            let (runner, place) = place_job(&self.config, name, &def, |image| {
                containerfile_at(&mirror, &sha, image)
            })?;
            planned.push(Planned { def, runner, place });
        }
        Ok(Some(planned))
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
            pipeline.finished = Some(crate::now_secs());
            pipeline.save(&self.state_dir, &self.changes);
            eprintln!(
                "[ci] {} #{} {}",
                pipeline.repo,
                pipeline.number,
                pipeline.status.as_str()
            );
            if pipeline.status == PipelineStatus::Failed {
                self.alert_red(&pipeline);
            }
            self.ready_since.retain(|(repo, number, _), _| {
                !(*repo == pipeline.repo && *number == pipeline.number)
            });
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

    /// Whether a job of `repo` named `job` is active on `runner` in any
    /// pipeline: one workspace, one job at a time.
    fn workspace_busy(&self, repo: &str, job: &str, runner: &str) -> bool {
        self.running.iter().any(|p| {
            p.repo == repo
                && p.jobs.iter().any(|j| {
                    j.def.name == job && j.status.is_active() && j.runner.as_deref() == Some(runner)
                })
        })
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
        let pipeline = self.running[pi].clone();
        self.post(
            &pipeline,
            &name,
            State::Success,
            format!("skipped: {note} (#{})", pipeline.number),
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

        // Inputs: what a need left, from this runner's own disk when the need
        // ran here, else from the CI once the need's runner uploaded it.
        let mut inputs = Vec::new();
        for need in &job.def.needs {
            let Some(needed) = pipeline.jobs.iter().find(|j| j.def.name == *need) else {
                continue;
            };
            if !needed.artifacts {
                continue;
            }
            let run = run_id(&pipeline.repo, pipeline.number, need);
            let source = if needed.runner.as_deref() == Some(runner.as_str()) {
                InputSource::Local { run }
            } else {
                let tar = artifacts_dir(&self.state_dir, &pipeline.repo, pipeline.number)
                    .join(format!("{need}.tar"));
                if !tar.exists() {
                    return self.set_waiting(pi, ji, format!("waiting for artifacts of {need}"));
                }
                InputSource::Ci { run }
            };
            inputs.push(Input {
                need: need.clone(),
                source,
            });
        }

        let key = (pipeline.repo.clone(), pipeline.number, name.clone());
        let ready = *self.ready_since.entry(key.clone()).or_insert(now);
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
        if self.workspace_busy(&pipeline.repo, &name, &runner) {
            return;
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
    /// environment and the secrets granted for the event.
    fn build_assignment(
        &self,
        pipeline: &Pipeline,
        ji: usize,
        place: &Place,
        inputs: Vec<Input>,
    ) -> Result<Assignment, String> {
        let job = &pipeline.jobs[ji].def;
        let repo = self
            .config
            .repos
            .get(&pipeline.repo)
            .ok_or_else(|| format!("repo {} is no longer configured", pipeline.repo))?;
        let sha = pipeline.sha.clone().unwrap_or_default();

        // Secrets: only those granted for this event.
        let mut secrets = Vec::new();
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
        let fetch_token = repo
            .fetch_token
            .as_ref()
            .map(|name| self.secrets.get(name).map_err(|e| e.to_string()))
            .transpose()?;

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
            url: repo.url.clone(),
            fetch_user: repo.fetch_user.clone(),
            fetch_token,
            sha,
            file: job.file.clone(),
            interpreter: job.interpreter.clone(),
            place: place.clone(),
            cache: job.cache.clone(),
            env,
            secrets,
            timeout_minutes: job.timeout_minutes,
            when_busy: job.when_busy,
            inputs,
        })
    }

    // ------------------------------------------------------------ runner messages

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::RunnerHello { name, runs } => self.runner_hello(&name, runs),
            Msg::RunnerGone { name } => eprintln!("[ci] runner {name} disconnected"),
            Msg::FromRunner { name, message } => self.on_runner_message(&name, message),
        }
    }

    /// A runner (re)connected and says which runs it knows. That settles
    /// every job that was active on it, and every run it still holds.
    fn runner_hello(&mut self, name: &str, runs: Vec<RunReport>) {
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
                }
            } else {
                // A run nobody here waits for: stop it and let it go.
                let run = report.run;
                self.runners.send(name, ToRunner::Kill { run: run.clone() });
                self.runners.unassign(&run);
                self.runners.send(name, ToRunner::Forget { run });
            }
        }
        self.resend_uploads(name);
    }

    /// Asks `runner` again for artifacts a job elsewhere is waiting for: the
    /// upload may have been lost with a connection.
    fn resend_uploads(&self, runner: &str) {
        for pipeline in &self.running {
            for job in &pipeline.jobs {
                if job.status != JobStatus::Succeeded
                    || !job.artifacts
                    || job.runner.as_deref() != Some(runner)
                {
                    continue;
                }
                let tar = artifacts_dir(&self.state_dir, &pipeline.repo, pipeline.number)
                    .join(format!("{}.tar", job.def.name));
                let wanted = pipeline.jobs.iter().any(|other| {
                    other.def.needs.contains(&job.def.name)
                        && other.runner.as_deref() != Some(runner)
                        && matches!(other.status, JobStatus::Pending | JobStatus::Waiting)
                });
                if wanted && !tar.exists() {
                    self.runners.send(
                        runner,
                        ToRunner::Upload {
                            run: run_id(&pipeline.repo, pipeline.number, &job.def.name),
                        },
                    );
                }
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
                record.started = Some(crate::now_secs());
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
        }
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
        } else {
            (
                State::Failure,
                failure_note(&end, timeout_minutes),
                excerpt::extract(&dir),
            )
        };

        let record = &mut self.running[pi].jobs[ji];
        record.status = if succeeded {
            JobStatus::Succeeded
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
        let pipeline = self.running[pi].clone();
        let description = if succeeded {
            format!("{note} (#{number})")
        } else {
            self.failure_description(&pipeline, &job, &note)
        };
        self.post(&pipeline, &job, state, description);

        if let Some(runner) = record_runner {
            let elsewhere = pipeline.jobs.iter().any(|other| {
                other.def.needs.contains(&job) && other.runner.as_deref() != Some(runner.as_str())
            });
            if succeeded && end.artifacts && elsewhere {
                self.runners
                    .send(&runner, ToRunner::Upload { run: run.clone() });
            }
            self.runners
                .send(&runner, ToRunner::Forget { run: run.clone() });
        }
        self.runners.unassign(&run);
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
        self.running[pi].save(&self.state_dir, &self.changes);
    }
}

/// The cause line of a failed job's excerpt, unless the job holds secrets:
/// its excerpt is its own output, and a status description or an alert is
/// read by people the secrets are not for.
fn shareable_cause(job: &JobRecord) -> Option<String> {
    if !job.def.secrets.is_empty() {
        return None;
    }
    job.excerpt.as_deref().and_then(excerpt::cause)
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
        Target::Runner(runner) => {
            if is_ps1 {
                return Err(fail(format!(
                    "{what} is a runner; a .ps1 job cannot run there"
                )));
            }
            (runner, Place::Host)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const FP_1: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    const FP_2: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";

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
        }
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

    fn config() -> CiConfig {
        CiConfig::parse(&format!(
            "default_runner = \"tuco-pc\"\n\
             [runners.tuco-pc]\nfingerprint = \"{FP_1}\"\nmachines = [\"win11\"]\n\
             [runners.atik]\nfingerprint = \"{FP_2}\"\n\
             [platforms]\nlinux-amd64 = \"tuco-pc\"\nlinux-arm64 = \"atik\"\n\
             windows-amd64 = \"win11\"\n"
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
            cache: Vec::new(),
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        runners.insert(
            name,
            crate::runners::Connected {
                tx,
                offer: zeughaus_link::enlist::Offer {
                    os: "linux".to_owned(),
                    arch: "x86_64".to_owned(),
                    cpus: 2,
                    containers: false,
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

    #[test]
    fn an_input_from_another_runner_waits_for_its_upload() {
        let state = scratch("remote-input");
        let mut s = scheduler(&state);
        let mut y = connect(&s.runners, "y", 0);
        s.running.push(two_jobs("x"));

        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Waiting);
        assert_eq!(s.running[0].jobs[1].note, "waiting for artifacts of a");
        assert!(y.try_recv().is_err());

        let dir = artifacts_dir(&state, "view", 7);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.tar"), b"tar").unwrap();
        s.advance();
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Starting);
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
        s.running.push(p);
        s.runners.assign("view.7.b", "y");
        s.handle(Msg::RunnerHello {
            name: "y".to_owned(),
            runs: Vec::new(),
        });
        assert_eq!(s.running[0].jobs[1].status, JobStatus::Pending);
        assert_eq!(s.runners.assigned_to("view.7.b"), None);

        // Handed over after the connection began: the runner has it queued.
        s.running[0].jobs[1].status = JobStatus::Starting;
        s.running[0].jobs[1].started = Some(150);
        s.handle(Msg::RunnerHello {
            name: "y".to_owned(),
            runs: Vec::new(),
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
    fn a_finished_job_leaves_its_exit_and_asks_for_the_upload_another_runner_needs() {
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
            Some(ToRunner::Upload {
                run: "view.7.a".to_owned()
            })
        );
        assert_eq!(
            x.try_recv().ok(),
            Some(ToRunner::Forget {
                run: "view.7.a".to_owned()
            })
        );
        assert_eq!(s.runners.assigned_to("view.7.a"), None);
        let _ = std::fs::remove_dir_all(&state);
    }
}
