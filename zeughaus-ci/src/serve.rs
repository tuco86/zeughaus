//! The `/ci` service: what editors ask the CI about its pipelines (see
//! [`zeughaus_link::ci`]).
//!
//! Channels and pipelines are read from the records on disk, which the
//! scheduler writes before it acts on a change: a request never waits for
//! the scheduler, and a restarted scheduler changes nothing here. The
//! runners come from the registry the `/enlist` service keeps. A job's
//! transcript is a read-only screen in [`Transcripts`], served on `/mux`.
//!
//! A `Repo` request also reads the jobs of a repository's `.zeughaus-ci/`
//! from the mirror the scheduler fetches into, with git, as reads only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use weida::{Replier, TransferMeta};
use zeughaus_link::ci::{
    LineSpan, MAX_ORIGIN_BYTES, MAX_PIPELINES_PER_REQUEST, MAX_TIMING_LINES, TIMING_READ_BYTES,
};
use zeughaus_link::enlist::Place;
use zeughaus_link::{
    BusyMode, ChannelView, CiOverview, CiReply, CiRequest, DocState, JobDefView, JobTiming,
    JobView, MAX_CI_REPLY_BYTES, MAX_CI_REQUEST_BYTES, MachineJob, MachineView, PipelineView,
    QueuedView, RepoView, TimedLine, TransferView,
};
use zeughaus_mux::TerminalId;
use zeughaus_terminal::wal;

use crate::CI_DIR;
use crate::config::{CiConfig, Forge, RepoConfig, valid_name};
use crate::docs;
use crate::event::{self, EventKind};
use crate::excerpt;
use crate::header::{JobDef, when_busy_name};
use crate::runners::Runners;
use crate::scheduler::{
    self, JobRecord, Pipeline, PipelineStatus, load_pipeline, load_pipelines, load_repo_pipelines,
    transcript_dir,
};
use crate::transcripts::Transcripts;
use crate::transport::editor_principal;

/// The most lines of a job's output one `Log` reply carries.
const MAX_LOG_LINES: u32 = 5000;

/// The most text one `Log` reply carries: the JSON around it and its escapes
/// take the rest of [`MAX_CI_REPLY_BYTES`].
const MAX_LOG_TEXT_BYTES: usize = 3 << 20;

/// How many of a repository's newest records a `Find` looks at.
const FIND_RECORDS: usize = 500;

/// What a `Find` or `Repo` answers to a `sha` that is not 4 to 40 hex digits.
const BAD_SHA: &str = "sha must be 4 to 40 hex digits";

/// The longest line of output a `Timing` reply carries, in characters.
const TIMING_TEXT_CHARS: usize = 200;

pub struct CiServer {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    runners: Arc<Runners>,
    transcripts: Arc<Transcripts>,
    /// Held by every request that changes a document: requests run on the
    /// blocking pool, and numbering and the one-plan-in-progress rule must
    /// not race.
    docs: std::sync::Mutex<()>,
    /// The repository of each document change, for the service to announce
    /// on `/events`.
    docs_changed: std::sync::mpsc::Sender<String>,
}

impl CiServer {
    pub fn new(
        state_dir: PathBuf,
        config: Arc<CiConfig>,
        runners: Arc<Runners>,
        transcripts: Arc<Transcripts>,
        docs_changed: std::sync::mpsc::Sender<String>,
    ) -> CiServer {
        CiServer {
            state_dir,
            config,
            runners,
            transcripts,
            docs: std::sync::Mutex::new(()),
            docs_changed,
        }
    }

    /// Blocking: reads records from disk, and for a `Repo` runs git on the
    /// scheduler's mirror.
    fn answer(&self, request: CiRequest) -> CiReply {
        match request {
            CiRequest::Overview => CiReply::Overview {
                overview: self.overview(),
            },
            CiRequest::Pipelines {
                repo,
                channel,
                before,
                limit,
            } => CiReply::Pipelines {
                pipelines: page(self.records(&repo), &channel, before, limit),
            },
            CiRequest::Pipeline { repo, number } => CiReply::Pipeline {
                pipeline: self
                    .record(&repo, number)
                    .as_ref()
                    .map(|record| Box::new(pipeline_view(record))),
            },
            CiRequest::OpenTranscript { repo, number, job } => {
                self.open_transcript(&repo, number, &job)
            }
            CiRequest::CloseTranscript { terminal } => {
                self.transcripts.close(TerminalId(terminal));
                CiReply::Closed
            }
            CiRequest::Find { repo, sha } => self.find(&repo, &sha),
            CiRequest::Rerun { repo, number } => self.rerun(&repo, number),
            CiRequest::Cancel { repo, number } => self.cancel(&repo, number),
            CiRequest::Log {
                repo,
                number,
                job,
                tail,
                timestamps,
                lines,
            } => self.log(&repo, number, &job, tail, timestamps, &lines),
            CiRequest::Timing {
                repo,
                number,
                job,
                top,
            } => self.timing(&repo, number, &job, top),
            CiRequest::Repo { origin, sha, jobs } => self.repo(&origin, sha.as_deref(), jobs),
            CiRequest::Docs { repo } => CiReply::Docs {
                docs: docs::list(&self.state_dir, &self.config, &repo),
            },
            CiRequest::Doc { repo, number } => CiReply::Doc {
                doc: docs::load(&self.state_dir, &self.config, &repo, number).map(Box::new),
            },
            CiRequest::DocCreate {
                repo,
                kind,
                title,
                body,
                source,
                proposals,
            } => {
                let new = docs::NewDoc {
                    kind,
                    title: &title,
                    body: &body,
                    source: &source,
                    proposals: &proposals,
                };
                let made = self.change_docs(&repo, |state, config, now| {
                    docs::create(state, config, &repo, new, now).map(|number| (number, true))
                });
                match made {
                    Ok(number) => CiReply::DocCreated { number },
                    Err(e) => error(&e),
                }
            }
            CiRequest::DocState {
                repo,
                number,
                state,
            } => accepted(self.change_docs(&repo, |dir, config, now| {
                docs::set_state(dir, config, &repo, number, state, now).map(|()| ((), true))
            })),
            CiRequest::DocDelete { repo, number } => {
                accepted(self.change_docs(&repo, |dir, config, _| {
                    docs::delete(dir, config, &repo, number).map(|()| ((), true))
                }))
            }
            // A master asks every few seconds; only a claim that took a plan
            // is announced.
            CiRequest::PlanClaim { repo } => {
                match self.change_docs(&repo, |dir, config, now| {
                    docs::claim(dir, config, &repo, now).map(|doc| {
                        let took = doc.is_some();
                        (doc, took)
                    })
                }) {
                    Ok(doc) => CiReply::Doc {
                        doc: doc.map(Box::new),
                    },
                    Err(e) => error(&e),
                }
            }
        }
    }

    /// Runs one change of `repo`'s documents under the lock and announces
    /// it when the change says it changed something.
    fn change_docs<T>(
        &self,
        repo: &str,
        change: impl FnOnce(&Path, &CiConfig, u64) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        let _held = self
            .docs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (done, changed) = change(&self.state_dir, &self.config, crate::now_secs())?;
        if changed {
            let _ = self.docs_changed.send(repo.to_owned());
        }
        Ok(done)
    }

    /// The records of `repo`. The name comes from a request and is joined
    /// into a path, so one no repository can have never reaches the disk.
    fn records(&self, repo: &str) -> Vec<Pipeline> {
        if valid_name(repo) {
            load_repo_pipelines(&self.state_dir, repo)
        } else {
            Vec::new()
        }
    }

    fn record(&self, repo: &str, number: u64) -> Option<Pipeline> {
        if !valid_name(repo) {
            return None;
        }
        load_pipeline(&self.state_dir, repo, number).ok()
    }

    fn overview(&self) -> CiOverview {
        // One pass over every record feeds all three parts.
        let pipelines = load_pipelines(&self.state_dir);
        let queued = event::queued(&self.state_dir)
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>();
        CiOverview {
            channels: channel_views(&pipelines),
            machines: self.machine_views(&pipelines),
            queued: queued_views(&pipelines, &queued),
            platforms: self.platforms(),
            repos: docs::summary(&self.state_dir, &self.config),
        }
    }

    /// The `[platforms]` of the configuration: each platform and the runner
    /// or machine that builds it.
    fn platforms(&self) -> Vec<(String, String)> {
        self.config
            .platforms
            .iter()
            .map(|(platform, target)| (platform.clone(), target.clone()))
            .collect()
    }

    /// Every configured runner by name (offline ones too), then the
    /// machines the connected ones report, each with the jobs running on it.
    fn machine_views(&self, pipelines: &[Pipeline]) -> Vec<MachineView> {
        let active = active_jobs(pipelines);
        let connected = self.runners.views();
        let mut runners = Vec::new();
        let mut machines = Vec::new();
        for name in self.config.runners.keys() {
            let jobs = active
                .iter()
                .filter(|a| a.runner.as_deref() == Some(name) && a.machine.is_none())
                .map(|a| a.job.clone())
                .collect();
            let status = match connected.get(name) {
                None => "offline".to_owned(),
                Some(view) => match &view.busy {
                    Some(state) => {
                        let mut status = if state.busy { "busy" } else { "free" }.to_owned();
                        if state.mode != BusyMode::Auto {
                            status.push_str(" (set)");
                        }
                        status
                    }
                    None => "online".to_owned(),
                },
            };
            runners.push(MachineView {
                name: name.clone(),
                kind: "runner".to_owned(),
                status,
                jobs,
            });
            for (machine, status) in connected.get(name).iter().flat_map(|v| &v.machines) {
                machines.push(MachineView {
                    name: machine.clone(),
                    kind: "windows-vm".to_owned(),
                    status: status.clone(),
                    jobs: active
                        .iter()
                        .filter(|a| {
                            a.runner.as_deref() == Some(name)
                                && a.machine.as_deref() == Some(machine)
                        })
                        .map(|a| a.job.clone())
                        .collect(),
                });
            }
        }
        runners.extend(machines);
        runners
    }

    /// A job's transcript screen. One with no output stored yet is still
    /// opened while the job is active: the screen fills as output arrives.
    fn open_transcript(&self, repo: &str, number: u64, job: &str) -> CiReply {
        let Some(pipeline) = self.record(repo, number) else {
            return error("no such job");
        };
        let Some(record) = pipeline.jobs.iter().find(|j| j.def.name == job) else {
            return error("no such job");
        };
        let stored = transcript_dir(&self.state_dir, repo, number, job).join("wal");
        if !stored.exists() && !record.status.is_active() {
            return error(&format!("{job} has no transcript"));
        }
        CiReply::Transcript {
            terminal: self.transcripts.open(repo, number, job).0,
        }
    }

    /// Queues the event of pipeline `number` again, pinned to the sha it
    /// ran for, so a branch that moved since does not change what runs.
    fn rerun(&self, repo: &str, number: u64) -> CiReply {
        let Some(pipeline) = self.record(repo, number) else {
            return error("no such pipeline");
        };
        // Its working trees are gone with the run: only the uploader has them.
        if pipeline.event.kind == EventKind::Manual {
            return error("a manual run cannot be rerun; start a new one");
        }
        let mut event = pipeline.event.clone();
        if pipeline.sha.is_some() {
            event.sha = pipeline.sha.clone();
        }
        event.actor = format!("rerun of #{number}");
        event.delivery = None;
        event.received = crate::now_secs();
        match event::write_inbox(&self.state_dir, &event) {
            Ok(_) => CiReply::Accepted,
            Err(e) => error(&e),
        }
    }

    /// Leaves a cancel request for the scheduler, which acts on its next
    /// tick.
    fn cancel(&self, repo: &str, number: u64) -> CiReply {
        let Some(pipeline) = self.record(repo, number) else {
            return error("no such pipeline");
        };
        if pipeline.status != PipelineStatus::Running {
            return error("the pipeline is not running");
        }
        match event::write_cancel(&self.state_dir, repo, number) {
            Ok(()) => CiReply::Accepted,
            Err(e) => error(&e),
        }
    }

    /// The newest pipeline of `repo`, of any channel, whose commit id starts
    /// with `sha`. Only the file names are listed; a record is read when its
    /// turn comes, newest first, so a recent push costs one read.
    fn find(&self, repo: &str, sha: &str) -> CiReply {
        if !valid_name(repo) {
            return error("no such repository");
        }
        let Some(sha) = sha_prefix(sha) else {
            return error(BAD_SHA);
        };
        let dir = crate::ci_dir(&self.state_dir).join("pipelines").join(repo);
        let mut numbers: Vec<u64> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                name.to_str()?.strip_suffix(".json")?.parse().ok()
            })
            .collect();
        numbers.sort_unstable_by_key(|&number| std::cmp::Reverse(number));
        numbers.truncate(FIND_RECORDS);
        let found = numbers
            .into_iter()
            .filter_map(|number| load_pipeline(&self.state_dir, repo, number).ok())
            .find(|pipeline| has_sha(pipeline, &sha));
        CiReply::Pipeline {
            pipeline: found.as_ref().map(|record| Box::new(pipeline_view(record))),
        }
    }

    /// The repository of `ci.toml` whose url `origin`, the origin remote of
    /// an agent's checkout, names: where its default branch stands, the
    /// newest pipeline of the checkout's commit `sha` and, with `jobs`, the
    /// jobs of its `.zeughaus-ci/` and where each would run. Records and the
    /// scheduler's mirror are only read, so the answer never waits for the
    /// scheduler.
    fn repo(&self, origin: &str, sha: Option<&str>, jobs: bool) -> CiReply {
        if origin.is_empty() || origin.len() > MAX_ORIGIN_BYTES {
            return error(&format!("origin must be 1 to {MAX_ORIGIN_BYTES} bytes"));
        }
        let sha = match sha {
            Some(sha) => match sha_prefix(sha) {
                Some(sha) => Some(sha),
                None => return error(BAD_SHA),
            },
            None => None,
        };
        let Ok((name, repo)) = crate::manual::repo_of(&self.config, origin) else {
            return CiReply::Repo { repo: None };
        };
        // Only this repository's records are read: the jobs a machine shows
        // are the ones this repository runs on it.
        let pipelines = load_repo_pipelines(&self.state_dir, name);
        let read = if jobs {
            self.read_jobs(name, &repo.default_branch, sha.as_deref())
        } else {
            RepoJobs::default()
        };
        CiReply::Repo {
            repo: Some(Box::new(RepoView {
                name: name.to_owned(),
                forge: forge_label(repo),
                default_branch: repo.default_branch.clone(),
                branch: crate::streak::of_branch(&pipelines, &repo.default_branch)
                    .describe(crate::now_secs()),
                channels: channel_views(&pipelines),
                head: sha
                    .and_then(|sha| newest_of_sha(&pipelines, &sha))
                    .map(|head| Box::new(pipeline_view(head))),
                jobs_sha: read.sha,
                jobs: read.jobs,
                jobs_note: read.note,
                machines: self.machine_views(&pipelines),
                platforms: self.platforms(),
                docs: docs::list(&self.state_dir, &self.config, name)
                    .into_iter()
                    .filter(|doc| {
                        matches!(
                            doc.state,
                            DocState::Open
                                | DocState::Saved
                                | DocState::Approved
                                | DocState::InProgress
                        )
                    })
                    .collect(),
            })),
        }
    }

    /// The jobs of `name`'s `.zeughaus-ci/` from the scheduler's mirror: at
    /// `sha` when the mirror has that commit, else at the head of
    /// `default_branch`, each with the place the CI would run it at.
    fn read_jobs(&self, name: &str, default_branch: &str, sha: Option<&str>) -> RepoJobs {
        let nothing = |note: String| RepoJobs {
            note,
            ..RepoJobs::default()
        };
        let mirror = scheduler::mirror_dir(&self.state_dir, name);
        if !mirror.join("HEAD").exists() {
            return nothing(format!(
                "the CI has not fetched {name} yet; it does with its first event"
            ));
        }
        let resolve = |rev: String| scheduler::rev_parse(&mirror, &rev).ok();
        let Some(commit) = sha
            .and_then(|sha| resolve(format!("{sha}^{{commit}}")))
            .or_else(|| resolve(format!("refs/heads/{default_branch}^{{commit}}")))
        else {
            return nothing(format!("no commit of {name} to read {CI_DIR}/ from"));
        };
        let short = &commit[..commit.len().min(7)];
        let read = scheduler::read_ci(&mirror, &commit).and_then(|(files, containerfiles)| {
            crate::pipeline::load_jobs(&files, &containerfiles)
        });
        let (jobs, note) = match read {
            Err(e) => (Vec::new(), format!("{short} {CI_DIR}/: {e}")),
            Ok(defs) if defs.is_empty() => (Vec::new(), format!("{short} has no job in {CI_DIR}/")),
            Ok(defs) => {
                let jobs: Vec<JobDefView> = defs
                    .iter()
                    .map(|def| {
                        job_def_view(&self.config, name, def, |image| {
                            scheduler::containerfile_at(&mirror, &commit, image)
                        })
                    })
                    .collect();
                (jobs, String::new())
            }
        };
        RepoJobs {
            sha: Some(commit),
            jobs,
            note,
        }
    }

    /// A job's stored output as numbered plain text: the last `tail` lines,
    /// or the `lines` asked for, never more than a reply may carry. The
    /// transcript is read from its end, so a job that wrote for hours costs
    /// what its last [`TIMING_READ_BYTES`] do. With `timestamps` every line
    /// also says when it began.
    fn log(
        &self,
        repo: &str,
        number: u64,
        job: &str,
        tail: u32,
        timestamps: bool,
        lines: &[LineSpan],
    ) -> CiReply {
        if !valid_name(repo) || !valid_name(job) {
            return error("no such job");
        }
        let dir = transcript_dir(&self.state_dir, repo, number, job);
        if !dir.join("wal").exists() {
            return error(&format!("no output for {job} of #{number}"));
        }
        // Read the same way in both modes, so a line keeps its number
        // whether or not timestamps were asked for.
        let output = match timed_tail(&dir, TIMING_READ_BYTES) {
            Ok(output) => output,
            Err(e) => return error(&e),
        };
        let keep_end = lines.is_empty();
        let last = [LineSpan::Last {
            count: tail.min(MAX_LOG_LINES),
        }];
        let spans = if keep_end { &last[..] } else { lines };
        let picked = pick_lines(output.lines.len(), spans);
        CiReply::Log {
            text: numbered(&output, &picked, timestamps, keep_end),
            finished: dir.join("exit").exists(),
        }
    }

    /// Where a job's time went, from the times its output arrived on the
    /// runner: the lines that the longest pauses follow.
    fn timing(&self, repo: &str, number: u64, job: &str, top: u32) -> CiReply {
        if !valid_name(repo) || !valid_name(job) {
            return error("no such job");
        }
        let Some(record) = self
            .record(repo, number)
            .and_then(|pipeline| pipeline.jobs.into_iter().find(|j| j.def.name == job))
        else {
            return error("no such job");
        };
        let dir = transcript_dir(&self.state_dir, repo, number, job);
        if !dir.join("wal").exists() {
            return error(&format!("no output for {job} of #{number}"));
        }
        let tail = match timed_tail(&dir, TIMING_READ_BYTES) {
            Ok(tail) => tail,
            Err(e) => return error(&e),
        };
        // The record knows when the job ended; one that still runs ends now.
        let end = record
            .finished
            .map_or_else(wal::now_micros, |secs| secs.saturating_mul(1_000_000));
        CiReply::Timing {
            timing: job_timing(&tail, end, dir.join("exit").exists(), top),
        }
    }
}

fn error(message: &str) -> CiReply {
    CiReply::Error {
        message: message.to_owned(),
    }
}

fn accepted(result: Result<(), String>) -> CiReply {
    match result {
        Ok(()) => CiReply::Accepted,
        Err(e) => error(&e),
    }
}

/// `sha` in lower case, when it is 4 to 40 hex digits: a commit id or the
/// start of one.
fn sha_prefix(sha: &str) -> Option<String> {
    ((4..=40).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| sha.to_ascii_lowercase())
}

/// Whether the commit `pipeline` ran for starts with `sha`, which is in
/// lower case.
fn has_sha(pipeline: &Pipeline, sha: &str) -> bool {
    pipeline
        .sha
        .as_deref()
        .and_then(|full| full.get(..sha.len()))
        .is_some_and(|start| start.eq_ignore_ascii_case(sha))
}

/// The newest of `pipelines`, one repository's records in any order, whose
/// commit starts with `sha`, which is in lower case.
fn newest_of_sha<'a>(pipelines: &'a [Pipeline], sha: &str) -> Option<&'a Pipeline> {
    pipelines
        .iter()
        .filter(|pipeline| has_sha(pipeline, sha))
        .max_by_key(|pipeline| pipeline.number)
}

/// `<forge> <slug>`, or `none` for a repository without a forge.
fn forge_label(repo: &RepoConfig) -> String {
    match repo.forge {
        Forge::Github => format!("github {}", repo.slug),
        Forge::Forgejo => format!("forgejo {}", repo.slug),
        Forge::None => "none".to_owned(),
    }
}

/// The jobs of a repository's `.zeughaus-ci/` as [`CiServer::read_jobs`]
/// found them.
#[derive(Default)]
struct RepoJobs {
    /// The commit they were read from.
    sha: Option<String>,
    jobs: Vec<JobDefView>,
    /// Why there are none; empty when there are.
    note: String,
}

/// A job of a `.zeughaus-ci/` and where the CI would run it. One that
/// cannot be placed says why and runs nowhere. `containerfile` reads the
/// text of `<image>.Containerfile` at the commit the job comes from.
fn job_def_view(
    config: &CiConfig,
    repo: &str,
    def: &JobDef,
    containerfile: impl Fn(&str) -> Result<String, String>,
) -> JobDefView {
    let (runner, at, placement, manual) =
        match scheduler::place_job(config, repo, def, containerfile) {
            // A machine is a Windows VM; a manual run does not take one.
            Ok((runner, place)) => {
                let manual = !matches!(place, Place::Machine { .. });
                (runner, place_at(&place), String::new(), manual)
            }
            Err(why) => (String::new(), String::new(), why, false),
        };
    JobDefView {
        name: def.name.clone(),
        file: def.file.clone(),
        on: def.on.clone(),
        needs: def.needs.clone(),
        channels: def.channels.clone(),
        place: scheduler::place_name(def),
        runner,
        at,
        placement,
        secrets: def.secrets.clone(),
        when_busy: when_busy_name(def.when_busy).to_owned(),
        manual,
    }
}

/// Where on its runner a job runs: `host`, `image:<x>` or `machine:<vm>`.
fn place_at(place: &Place) -> String {
    match place {
        Place::Host => "host".to_owned(),
        Place::Container { image, .. } => format!("image:{image}"),
        Place::Machine { name } => format!("machine:{name}"),
    }
}

/// The lines of a transcript's last bytes, with the times they began.
struct TimedTail {
    /// Microseconds since the UNIX epoch and the text of each line that has
    /// text, oldest first.
    lines: Vec<(u64, String)>,
    /// When the transcript's first output arrived; `None` while it has none.
    origin: Option<u64>,
    /// The output before these lines was not read.
    truncated: bool,
}

/// The lines of the transcript in `dir` that its last `max` bytes hold. A
/// window that opens inside the output starts with the rest of a line whose
/// beginning was left out, which is dropped.
fn timed_tail(dir: &Path, max: usize) -> Result<TimedTail, String> {
    let path = dir.join("wal");
    let unreadable =
        |e: std::io::Error| format!("cannot read the output of {}: {e}", dir.display());
    let (records, truncated) = wal::tail_records(&path, max).map_err(unreadable)?;
    let origin = wal::first_at(&path).map_err(unreadable)?;
    let mut lines = excerpt::timed_lines(&records);
    if truncated && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(TimedTail {
        lines,
        origin,
        truncated,
    })
}

/// The lines `spans` select out of `total`, as 0-based indices: ascending,
/// each once. A part past the end selects nothing; a range that runs past
/// it ends there.
fn pick_lines(total: usize, spans: &[LineSpan]) -> Vec<usize> {
    let mut picked = Vec::new();
    for span in spans {
        let (first, last) = match *span {
            LineSpan::Range { first, last } => (
                (first as usize).max(1),
                last.map_or(total, |last| (last as usize).min(total)),
            ),
            LineSpan::Last { count } => (total.saturating_sub(count as usize) + 1, total),
        };
        if first <= last {
            picked.extend(first - 1..last);
        }
    }
    picked.sort_unstable();
    picked.dedup();
    picked
}

/// The lines of `tail` at `picked` (from [`pick_lines`]) as text, each as
/// `<number>| <text>` with the numbers right-aligned to the widest one
/// shown, with `timestamps` as `<number>| [+<seconds>s] <text>` (seconds
/// since the job's first output), and `...` between lines that are not
/// adjacent. At most [`MAX_LOG_LINES`] lines and [`MAX_LOG_TEXT_BYTES`]:
/// `keep_end` (a tail) drops lines from the front, otherwise lines are
/// dropped from the end and a note says where to continue.
fn numbered(tail: &TimedTail, picked: &[usize], timestamps: bool, keep_end: bool) -> String {
    let mut text = String::new();
    if tail.truncated {
        text.push_str(&format!(
            "(only the last {} MiB of output were read; line numbers count from there)\n",
            TIMING_READ_BYTES >> 20
        ));
    }
    let Some(&widest) = picked.last() else {
        text.push_str(&format!(
            "(no such lines: the output has {} lines)\n",
            tail.lines.len()
        ));
        return text;
    };
    let digits = |index: usize| (index + 1).to_string().len();
    // What a line costs, measured with the widest number of all picked
    // lines; the stamp `[+    65.8s] ` is 13 bytes.
    let width = digits(widest);
    let stamp = if timestamps { 13 } else { 0 };
    let cost = |k: usize| {
        let gap = k > 0 && picked[k - 1] + 1 != picked[k];
        width + 2 + stamp + tail.lines[picked[k]].1.len() + 1 + if gap { 4 } else { 0 }
    };
    let max_lines = MAX_LOG_LINES as usize;
    let mut bytes = 0;
    let (from, to) = if keep_end {
        let mut from = picked.len();
        while from > 0 && picked.len() - from < max_lines {
            let more = cost(from - 1);
            if from < picked.len() && bytes + more > MAX_LOG_TEXT_BYTES {
                break;
            }
            bytes += more;
            from -= 1;
        }
        (from, picked.len())
    } else {
        let mut to = 0;
        while to < picked.len() && to < max_lines {
            let more = cost(to);
            if to > 0 && bytes + more > MAX_LOG_TEXT_BYTES {
                break;
            }
            bytes += more;
            to += 1;
        }
        (0, to)
    };
    let kept = &picked[from..to];
    let width = kept.last().map_or(1, |&last| digits(last));
    let origin = tail.origin.unwrap_or(0);
    for (k, &index) in kept.iter().enumerate() {
        if k > 0 && kept[k - 1] + 1 != index {
            text.push_str("...\n");
        }
        let (at, line) = &tail.lines[index];
        // A single line longer than a reply is cut at a character.
        let mut end = line.len().min(MAX_LOG_TEXT_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let line = &line[..end];
        let number = index + 1;
        if timestamps {
            let seconds = at.saturating_sub(origin) as f64 / 1e6;
            text.push_str(&format!("{number:>width$}| [+{seconds:>8.1}s] {line}\n"));
        } else {
            text.push_str(&format!("{number:>width$}| {line}\n"));
        }
    }
    if to < picked.len() {
        text.push_str(&format!(
            "(cut at {} lines; continue with lines={}-)\n",
            kept.len(),
            picked[to] + 1
        ));
    }
    text
}

/// A job's lines as how long each stood: the `top` lines (clamped to
/// `1..=`[`MAX_TIMING_LINES`]) that the longest pauses follow, in output
/// order. A line stands until the next one begins and the last one until
/// `end`, in microseconds since the UNIX epoch; the end comes from the
/// CI's clock and the lines from the runner's, so it is never taken as
/// earlier than the last line.
fn job_timing(tail: &TimedTail, end: u64, finished: bool, top: u32) -> JobTiming {
    let lines = &tail.lines;
    let end = lines.last().map_or(end, |&(at, _)| end.max(at));
    let took: Vec<u64> = lines
        .iter()
        .zip(lines.iter().skip(1).map(|&(at, _)| at).chain([end]))
        .map(|(&(at, _), next)| next.saturating_sub(at))
        .collect();
    let keep = (top.clamp(1, MAX_TIMING_LINES) as usize).min(lines.len());
    let mut order: Vec<usize> = (0..lines.len()).collect();
    if keep < order.len() {
        // The earlier line wins a tie, so the pick does not depend on how
        // the selection happens to arrange equals.
        order.select_nth_unstable_by_key(keep, |&i| (std::cmp::Reverse(took[i]), i));
        order.truncate(keep);
    }
    order.sort_unstable();
    let origin = tail.origin;
    JobTiming {
        lines: lines.len() as u64,
        span_ms: origin.map_or(0, |from| end.saturating_sub(from) / 1000),
        slowest: order
            .into_iter()
            .map(|i| TimedLine {
                line: u32::try_from(i + 1).unwrap_or(u32::MAX),
                at_ms: lines[i].0.saturating_sub(origin.unwrap_or(0)) / 1000,
                took_ms: took[i] / 1000,
                text: lines[i].1.chars().take(TIMING_TEXT_CHARS).collect(),
            })
            .collect(),
        finished,
        truncated: tail.truncated,
    }
}

/// A page of `channel`'s pipelines, newest first: those numbered below
/// `before`, at most `limit` of them, clamped to `1..=`
/// [`MAX_PIPELINES_PER_REQUEST`].
fn page(
    records: Vec<Pipeline>,
    channel: &str,
    before: Option<u64>,
    limit: u32,
) -> Vec<PipelineView> {
    let mut records: Vec<Pipeline> = records
        .into_iter()
        .filter(|p| before.is_none_or(|before| p.number < before) && p.channel() == channel)
        .collect();
    records.sort_by_key(|p| std::cmp::Reverse(p.number));
    records.truncate(limit.clamp(1, MAX_PIPELINES_PER_REQUEST) as usize);
    records.iter().map(pipeline_view).collect()
}

fn pipeline_view(pipeline: &Pipeline) -> PipelineView {
    PipelineView {
        repo: pipeline.repo.clone(),
        number: pipeline.number,
        channel: pipeline.channel(),
        event: pipeline.event.kind.to_string(),
        git_ref: pipeline.event.git_ref.clone(),
        sha: pipeline.sha.clone(),
        actor: pipeline.event.actor.clone(),
        subject: pipeline.subject.clone(),
        status: pipeline.status.as_str().to_owned(),
        note: pipeline.note.clone(),
        created: pipeline.created,
        finished: pipeline.finished,
        jobs: pipeline.jobs.iter().map(job_view).collect(),
    }
}

fn job_view(job: &JobRecord) -> JobView {
    JobView {
        name: job.def.name.clone(),
        needs: job.def.needs.clone(),
        place: shown_place(job),
        status: job.status.as_str().to_owned(),
        note: job.note.clone(),
        started: job.started,
        finished: job.finished,
        code: job.code,
        cause: job.cause(),
        excerpt: job.excerpt.clone(),
        assigned: job.assigned,
        transfers: job
            .transfers
            .iter()
            .map(|t| TransferView {
                need: t.need.clone(),
                started: t.started,
                finished: t.finished,
                bytes: t.bytes,
                error: t.error.clone(),
            })
            .collect(),
    }
}

/// Where a job runs as its card shows it: the runner, and what on it.
/// Records without a placement show the header's place alone.
fn shown_place(job: &JobRecord) -> String {
    match (&job.runner, &job.place) {
        (Some(runner), Some(Place::Host)) => runner.clone(),
        (Some(runner), Some(Place::Machine { name })) => format!("{name} on {runner}"),
        (Some(runner), _) => format!("{} on {runner}", scheduler::place_name(&job.def)),
        (None, _) => scheduler::place_name(&job.def),
    }
}

/// Every channel of every repository with its newest pipeline, by
/// repository and then channel.
fn channel_views(pipelines: &[Pipeline]) -> Vec<ChannelView> {
    let mut newest: BTreeMap<(&str, String), &Pipeline> = BTreeMap::new();
    for pipeline in pipelines {
        let slot = newest
            .entry((pipeline.repo.as_str(), pipeline.channel()))
            .or_insert(pipeline);
        if pipeline.number > slot.number {
            *slot = pipeline;
        }
    }
    newest
        .into_iter()
        .map(|((repo, channel), pipeline)| ChannelView {
            repo: repo.to_owned(),
            channel,
            latest: pipeline.number,
            status: pipeline.status.as_str().to_owned(),
        })
        .collect()
}

/// The queued events, each in the channel of the newest pipeline that had
/// its queue key (the same repository, kind, ref and cron), else in the
/// channel its event names without a `channels.toml`.
fn queued_views(pipelines: &[Pipeline], queued: &[event::CiEvent]) -> Vec<QueuedView> {
    queued
        .iter()
        .map(|queued| {
            let key = event::queue_key(queued);
            let channel = pipelines
                .iter()
                .filter(|p| p.repo == queued.repo && p.key == key)
                .max_by_key(|p| p.number)
                .map_or_else(|| crate::channels::fallback(queued), Pipeline::channel);
            QueuedView {
                repo: queued.repo.clone(),
                channel,
                event: queued.kind.to_string(),
                git_ref: queued.git_ref.clone(),
                sha: queued.sha.clone(),
                actor: queued.actor.clone(),
                received: queued.received,
            }
        })
        .collect()
}

/// A job that is starting or running, with where.
struct ActiveJob {
    runner: Option<String>,
    /// The machine behind the runner, for a job placed on one.
    machine: Option<String>,
    job: MachineJob,
}

/// The jobs running now of the pipelines that are.
fn active_jobs(pipelines: &[Pipeline]) -> Vec<ActiveJob> {
    pipelines
        .iter()
        .filter(|p| p.status == PipelineStatus::Running)
        .flat_map(|p| {
            p.jobs
                .iter()
                .filter(|j| j.status.is_active())
                .map(move |j| ActiveJob {
                    runner: j.runner.clone(),
                    machine: match &j.place {
                        Some(Place::Machine { name }) => Some(name.clone()),
                        _ => None,
                    },
                    job: MachineJob {
                        repo: p.repo.clone(),
                        channel: p.channel.clone(),
                        number: p.number,
                        job: j.def.name.clone(),
                    },
                })
        })
        .collect()
}

/// A reply an editor will read: one larger than it reads is replaced by an
/// error that says so.
fn encode(reply: CiReply) -> Vec<u8> {
    let bytes = reply.encode();
    if bytes.len() <= MAX_CI_REPLY_BYTES {
        return bytes;
    }
    error("the answer is larger than a reply may be").encode()
}

/// Answers `/ci` requests until the replier goes away, which for this
/// process means never.
pub async fn serve(replier: Replier, server: Arc<CiServer>) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[ci] stopped serving /ci: {e}");
                return;
            }
        };
        if let Err(why) = editor_principal(&server.config, request.meta()) {
            eprintln!("[ci] refused a ci request from a peer that {why}");
            request.refuse(weida::ErrorCode::Rejected).await;
            continue;
        }
        let payload = match request.take_body().collect(MAX_CI_REQUEST_BYTES).await {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!("[ci] unreadable ci request: {e}");
                continue;
            }
        };
        let answer = match CiRequest::decode(&payload) {
            Some(asked) => {
                let server = Arc::clone(&server);
                // On the blocking pool: the records are on disk and a
                // transcript starts a process, while the task this runs on
                // is also weida's, which must keep draining the connection.
                tokio::task::spawn_blocking(move || server.answer(asked))
                    .await
                    .unwrap_or_else(|e| CiReply::Error {
                        message: format!("the request failed: {e}"),
                    })
            }
            None => error("malformed request"),
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] cannot reply to a ci request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&encode(answer)).await {
            eprintln!("[ci] cannot write a ci reply: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[ci] cannot finish a ci reply: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use zeughaus_link::enlist::{Offer, Place};

    use super::*;
    use crate::event::{CiEvent, EventKind};
    use crate::header::JobDef;
    use crate::scheduler::JobStatus;

    const FINGERPRINT: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000001";

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeughaus-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// The test configuration followed by `tables`: more of `ci.toml`.
    fn config(tables: &str) -> Arc<CiConfig> {
        let other = FINGERPRINT.replace('1', "2");
        Arc::new(
            CiConfig::parse(&format!(
                "default_runner = \"alpha\"\n\
                 [runners.alpha]\nfingerprint = \"{FINGERPRINT}\"\nmachines = [\"win11\"]\n\
                 [runners.beta]\nfingerprint = \"{other}\"\n\
                 [platforms]\nlinux-amd64 = \"alpha\"\nwindows-amd64 = \"win11\"\n{tables}"
            ))
            .expect("a valid ci.toml"),
        )
    }

    fn job(name: &str, status: JobStatus, runner: Option<&str>, place: Option<Place>) -> JobRecord {
        JobRecord {
            def: JobDef {
                name: name.to_owned(),
                file: format!("{name}.sh"),
                on: Vec::new(),
                needs: Vec::new(),
                image: None,
                machine: None,
                platform: None,
                ignored_keys: Vec::new(),
                channels: Vec::new(),
                secrets: Vec::new(),
                when_busy: Default::default(),
                env: Default::default(),
                timeout_minutes: 60,
                interpreter: "sh".to_owned(),
            },
            status,
            note: String::new(),
            code: None,
            started: None,
            assigned: None,
            transfers: Vec::new(),
            finished: None,
            runner: runner.map(str::to_owned),
            place,
            artifacts: false,
            excerpt: None,
        }
    }

    fn pipeline(
        repo: &str,
        number: u64,
        channel: &str,
        status: PipelineStatus,
        jobs: Vec<JobRecord>,
    ) -> Pipeline {
        Pipeline {
            repo: repo.to_owned(),
            number,
            key: String::new(),
            event: CiEvent {
                repo: repo.to_owned(),
                kind: EventKind::Push,
                git_ref: "main".to_owned(),
                sha: None,
                cron: None,
                actor: "test".to_owned(),
                delivery: None,
                upload: None,
                received: 0,
            },
            sha: Some(format!("{number:040}")),
            subject: String::new(),
            status,
            note: String::new(),
            created: number,
            finished: None,
            jobs,
            cancelled: false,
            clean: false,
            channel: channel.to_owned(),
        }
    }

    fn server(state: &Path) -> CiServer {
        server_with(state, "")
    }

    /// A server whose `ci.toml` also has `tables`.
    fn server_with(state: &Path, tables: &str) -> CiServer {
        CiServer::new(
            state.to_path_buf(),
            config(tables),
            Arc::new(Runners::new()),
            Arc::new(Transcripts::new(state.to_path_buf())),
            std::sync::mpsc::channel().0,
        )
    }

    /// A queued event lands in the channel its ref's pipelines went to, so a
    /// rerun of a `dev` pipeline shows under `dev`; a ref with none yet in
    /// the channel its event names.
    #[test]
    fn a_queued_event_is_shown_in_the_channel_of_its_refs_pipelines() {
        let mut dev = pipeline("griasdi", 4, "dev", PipelineStatus::Running, Vec::new());
        dev.key = event::queue_key(&dev.event);
        let mut rerun = dev.event.clone();
        rerun.actor = "rerun of #4".to_owned();
        let mut feature = dev.event.clone();
        feature.git_ref = "feature/x".to_owned();
        let shown = queued_views(&[dev], &[rerun, feature]);
        let channels: Vec<(&str, &str)> = shown
            .iter()
            .map(|q| (q.channel.as_str(), q.actor.as_str()))
            .collect();
        assert_eq!(channels, [("dev", "rerun of #4"), ("feature/x", "test")]);
    }

    #[test]
    fn a_channel_page_is_newest_first_and_bounded() {
        let records: Vec<Pipeline> = (1..=60)
            .map(|n| {
                let channel = if n <= 55 { "dev" } else { "release" };
                pipeline("view", n, channel, PipelineStatus::Succeeded, Vec::new())
            })
            .collect();
        let numbers = |views: Vec<PipelineView>| views.iter().map(|v| v.number).collect::<Vec<_>>();

        assert_eq!(numbers(page(records.clone(), "dev", None, 3)), [55, 54, 53]);
        assert_eq!(numbers(page(records.clone(), "dev", Some(53), 2)), [52, 51]);
        assert_eq!(
            numbers(page(records.clone(), "release", None, 10)),
            [60, 59, 58, 57, 56]
        );
        // The limit is the editor's wish, clamped to what a reply holds.
        assert_eq!(page(records.clone(), "dev", None, 0).len(), 1);
        assert_eq!(
            page(records.clone(), "dev", None, 1000).len(),
            MAX_PIPELINES_PER_REQUEST as usize
        );
        assert!(page(records, "nightly", None, 10).is_empty());
    }

    #[test]
    fn a_pipeline_is_viewed_with_its_jobs_in_record_order() {
        let mut failed = job("test", JobStatus::Failed, None, None);
        failed.def.needs = vec!["build".to_owned()];
        failed.def.machine = Some("mac".to_owned());
        failed.code = Some(3);
        failed.started = Some(10);
        failed.finished = Some(25);
        failed.excerpt = Some("compiling\nerror: boom".to_owned());
        failed.def.secrets = vec!["TOKEN".to_owned()];
        let record = pipeline(
            "view",
            3,
            "",
            PipelineStatus::Failed,
            vec![
                job("build", JobStatus::Succeeded, None, None),
                failed,
                job(
                    "lint",
                    JobStatus::Succeeded,
                    Some("alpha"),
                    Some(Place::Host),
                ),
                job(
                    "win",
                    JobStatus::Succeeded,
                    Some("alpha"),
                    Some(Place::Machine {
                        name: "win11".to_owned(),
                    }),
                ),
            ],
        );

        let view = pipeline_view(&record);
        // No channel stored: the one the event sorts into.
        assert_eq!(
            (
                view.channel.as_str(),
                view.event.as_str(),
                view.git_ref.as_str()
            ),
            ("main", "push", "main")
        );
        assert_eq!(view.status, "failed");
        assert_eq!(view.sha.as_deref().map(str::len), Some(40));
        let jobs: Vec<_> = view
            .jobs
            .iter()
            .map(|j| (j.name.as_str(), j.place.as_str(), j.status.as_str()))
            .collect();
        assert_eq!(
            jobs,
            [
                ("build", "host", "succeeded"),
                ("test", "machine:mac", "failed"),
                ("lint", "alpha", "succeeded"),
                ("win", "win11 on alpha", "succeeded"),
            ]
        );
        assert_eq!(view.jobs[1].needs, ["build"]);
        // The cause is shown even for a job that holds secrets; a job that
        // did not fail has none.
        assert_eq!(view.jobs[1].cause.as_deref(), Some("error: boom"));
        assert_eq!(
            view.jobs[1].excerpt.as_deref(),
            Some("compiling\nerror: boom")
        );
        assert_eq!(
            (view.jobs[0].cause.clone(), view.jobs[0].excerpt.clone()),
            (None, None)
        );
        assert_eq!(
            (
                view.jobs[1].code,
                view.jobs[1].started,
                view.jobs[1].finished
            ),
            (Some(3), Some(10), Some(25))
        );
    }

    #[test]
    fn a_commit_id_is_matched_by_its_start_whatever_the_case_of_the_record() {
        let mut record = pipeline("view", 1, "dev", PipelineStatus::Running, Vec::new());
        record.sha = Some("ABCDEF0123456789ABCDEF0123456789ABCDEF01".to_owned());
        assert!(has_sha(&record, "abcdef0"));
        assert!(has_sha(&record, "abcdef0123456789abcdef0123456789abcdef01"));
        assert!(!has_sha(&record, "abcdef1"));
        // Longer than any commit id.
        assert!(!has_sha(
            &record,
            "abcdef0123456789abcdef0123456789abcdef012"
        ));
        record.sha = None;
        assert!(!has_sha(&record, "abcd"));
    }

    #[test]
    fn an_image_job_is_placed_by_the_text_of_its_containerfile() {
        let mut def = job("check", JobStatus::Pending, None, None).def;
        def.image = Some("trixie".to_owned());
        let config = config("");

        let placed = job_def_view(&config, "view", &def, |image| {
            assert_eq!(image, "trixie");
            Ok("FROM scratch\n".to_owned())
        });
        assert_eq!(
            (placed.runner.as_str(), placed.at.as_str(), placed.manual),
            ("alpha", "image:trixie", true)
        );
        assert_eq!(placed.placement, "");

        let lost = job_def_view(&config, "view", &def, |_| {
            Err("git show: no such path".to_owned())
        });
        assert_eq!(
            (lost.runner.as_str(), lost.at.as_str(), lost.manual),
            ("", "", false)
        );
        assert_eq!(lost.placement, "git show: no such path");
    }

    #[test]
    fn a_line_stands_until_the_next_and_the_last_until_the_end_of_the_job() {
        let tail = |lines: &[(u64, &str)]| TimedTail {
            lines: lines
                .iter()
                .map(|&(at, text)| (at, text.to_owned()))
                .collect(),
            origin: Some(1_000_000),
            truncated: false,
        };
        let texts = |timing: &JobTiming| {
            timing
                .slowest
                .iter()
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
        };
        let even = tail(&[(1_000_000, "a"), (2_000_000, "b"), (3_000_000, "c")]);

        // Equal pauses go to the earlier line, whatever the selection does.
        let timing = job_timing(&even, 4_000_000, true, 2);
        assert_eq!(texts(&timing), ["a", "b"]);
        assert_eq!(
            (timing.lines, timing.span_ms, timing.finished),
            (3, 3_000, true)
        );
        assert_eq!(
            texts(&job_timing(&even, 4_000_000, true, 15)),
            ["a", "b", "c"]
        );

        // An end before the last line (the CI's clock is not the runner's)
        // leaves that line no time, not a negative one.
        let early = job_timing(&even, 2_500_000, true, 15);
        assert_eq!(
            early.slowest.last().map(|line| (line.at_ms, line.took_ms)),
            Some((2_000, 0))
        );
        assert_eq!(early.span_ms, 2_000);

        // A long line is cut at a character, not in the middle of one.
        let long = tail(&[(1_000_000, &"\u{e9}".repeat(300))]);
        let timing = job_timing(&long, 2_000_000, false, 1);
        assert_eq!(timing.slowest[0].text, "\u{e9}".repeat(TIMING_TEXT_CHARS));
        assert!(!timing.finished);

        // Output that has not begun has no span.
        let nothing = TimedTail {
            lines: Vec::new(),
            origin: None,
            truncated: true,
        };
        let timing = job_timing(&nothing, 9_000_000, false, 15);
        assert_eq!((timing.lines, timing.span_ms), (0, 0));
        assert!(timing.slowest.is_empty() && timing.truncated);
    }

    #[test]
    fn line_spans_pick_each_line_once_in_order_and_stop_at_the_end() {
        let range = |first, last| LineSpan::Range { first, last };
        assert_eq!(pick_lines(10, &[range(3, Some(5))]), [2, 3, 4]);
        assert_eq!(pick_lines(10, &[range(8, None)]), [7, 8, 9]);
        assert_eq!(pick_lines(10, &[LineSpan::Last { count: 2 }]), [8, 9]);
        assert_eq!(pick_lines(3, &[LineSpan::Last { count: 50 }]), [0, 1, 2]);
        // Overlapping and unordered spans give each line once, in order.
        assert_eq!(
            pick_lines(
                10,
                &[
                    LineSpan::Last { count: 1 },
                    range(3, Some(6)),
                    range(2, Some(4))
                ]
            ),
            [1, 2, 3, 4, 5, 9]
        );
        // Past the end: nothing, or up to the end.
        assert!(pick_lines(10, &[range(11, None)]).is_empty());
        assert_eq!(pick_lines(10, &[range(9, Some(20))]), [8, 9]);
        assert!(pick_lines(10, &[LineSpan::Last { count: 0 }]).is_empty());
        assert!(pick_lines(0, &[LineSpan::Last { count: 5 }]).is_empty());
        // A zero only a hand-made request can carry counts as line 1.
        assert_eq!(pick_lines(10, &[range(0, Some(2))]), [0, 1]);
    }

    /// `count` lines of `text`, half a second apart from the first output.
    fn output(count: usize, text: impl Fn(usize) -> String, truncated: bool) -> TimedTail {
        TimedTail {
            lines: (0..count)
                .map(|i| (1_000_000 + i as u64 * 500_000, text(i)))
                .collect(),
            origin: Some(1_000_000),
            truncated,
        }
    }

    #[test]
    fn numbered_lines_align_their_numbers_and_mark_gaps() {
        let letters = output(12, |i| ((b'a' + i as u8) as char).to_string(), false);
        assert_eq!(
            numbered(&letters, &[0, 1, 4], false, false),
            "1| a\n2| b\n...\n5| e\n"
        );
        // The width is that of the widest number shown.
        assert_eq!(
            numbered(&letters, &[8, 9, 10, 11], false, true),
            " 9| i\n10| j\n11| k\n12| l\n"
        );
        assert_eq!(
            numbered(&letters, &[1, 10], true, false),
            " 2| [+     0.5s] b\n...\n11| [+     5.0s] k\n"
        );
        assert_eq!(
            numbered(&letters, &[], false, false),
            "(no such lines: the output has 12 lines)\n"
        );
        let cut = output(2, |_| "x".to_owned(), true);
        assert_eq!(
            numbered(&cut, &[0, 1], false, true),
            "(only the last 64 MiB of output were read; line numbers count from there)\n\
             1| x\n2| x\n"
        );
    }

    #[test]
    fn numbered_lines_stop_at_the_reply_limits_from_the_side_that_matters() {
        let max = MAX_LOG_LINES as usize;
        let many = output(max + 3, |_| "x".to_owned(), false);
        let all: Vec<usize> = (0..max + 3).collect();

        // Asked for lines: the first ones, and where to go on.
        let head = numbered(&many, &all, false, false);
        let rows: Vec<&str> = head.lines().collect();
        assert_eq!(rows.len(), max + 1);
        assert_eq!(rows[0], "   1| x");
        assert_eq!(rows[max - 1], "5000| x");
        assert_eq!(rows[max], "(cut at 5000 lines; continue with lines=5001-)");

        // A tail: the last ones, nothing said.
        let tail = numbered(&many, &all, false, true);
        let rows: Vec<&str> = tail.lines().collect();
        assert_eq!(rows.len(), max);
        assert_eq!(rows[0], "   4| x");
        assert_eq!(rows[max - 1], "5003| x");

        // Long lines hit the byte limit before the line limit.
        let wide = output(4000, |_| "y".repeat(1000), false);
        let all: Vec<usize> = (0..4000).collect();
        let tail = numbered(&wide, &all, false, true);
        assert!(tail.len() <= MAX_LOG_TEXT_BYTES, "{}", tail.len());
        assert!(tail.ends_with(&format!("4000| {}\n", "y".repeat(1000))));
        assert!(!tail.starts_with("   1|"));
        let head = numbered(&wide, &all, false, false);
        assert!(head.len() <= MAX_LOG_TEXT_BYTES + 100, "{}", head.len());
        assert!(head.starts_with("   1| y"));
        assert!(head.contains("; continue with lines="));
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;
        use crate::runners::Connected;
        use zeughaus_link::MachineState;

        fn store(state: &Path, pipeline: &Pipeline) {
            let path = state
                .join("ci/pipelines")
                .join(&pipeline.repo)
                .join(format!("{}.json", pipeline.number));
            crate::files::write_json(&path, pipeline).expect("write the record");
        }

        fn connect(server: &CiServer, name: &str, busy: Option<MachineState>) {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            server.runners.insert(
                name,
                Connected {
                    tx,
                    offer: Offer {
                        os: "linux".to_owned(),
                        arch: "x86_64".to_owned(),
                        cpus: 4,
                        containers: false,
                        machines: Vec::new(),
                    },
                    busy,
                    machines: BTreeMap::new(),
                    since: 1,
                },
            );
        }

        fn open(server: &CiServer, repo: &str, number: u64, job: &str) -> CiReply {
            server.answer(CiRequest::OpenTranscript {
                repo: repo.to_owned(),
                number,
                job: job.to_owned(),
            })
        }

        #[test]
        fn the_overview_lists_channels_runners_and_what_runs_where() {
            let state = scratch("overview");
            let win11 = Place::Machine {
                name: "win11".to_owned(),
            };
            store(
                &state,
                &pipeline(
                    "view",
                    1,
                    "dev",
                    PipelineStatus::Succeeded,
                    vec![job("build", JobStatus::Succeeded, Some("alpha"), None)],
                ),
            );
            store(
                &state,
                &pipeline(
                    "view",
                    2,
                    "dev",
                    PipelineStatus::Running,
                    vec![
                        job(
                            "build",
                            JobStatus::Running,
                            Some("alpha"),
                            Some(Place::Host),
                        ),
                        job("windows", JobStatus::Starting, Some("alpha"), Some(win11)),
                        job("mac", JobStatus::Running, Some("beta"), Some(Place::Host)),
                        job("test", JobStatus::Pending, None, None),
                    ],
                ),
            );
            store(
                &state,
                &pipeline(
                    "view",
                    3,
                    "release",
                    PipelineStatus::Failed,
                    vec![job("build", JobStatus::Failed, Some("alpha"), None)],
                ),
            );
            store(
                &state,
                &pipeline("other", 1, "dev", PipelineStatus::Succeeded, Vec::new()),
            );
            let server = server(&state);
            connect(
                &server,
                "alpha",
                Some(MachineState {
                    busy: true,
                    mode: BusyMode::Busy,
                }),
            );
            server.runners.set_machine("alpha", "win11", "running");

            let overview = |server: &CiServer| match server.answer(CiRequest::Overview) {
                CiReply::Overview { overview } => overview,
                other => panic!("unexpected reply {other:?}"),
            };
            let seen = overview(&server);
            let channel = |repo: &str, channel: &str, latest: u64, status: &str| ChannelView {
                repo: repo.to_owned(),
                channel: channel.to_owned(),
                latest,
                status: status.to_owned(),
            };
            assert_eq!(
                seen.platforms,
                [
                    ("linux-amd64".to_owned(), "alpha".to_owned()),
                    ("windows-amd64".to_owned(), "win11".to_owned()),
                ]
            );
            assert_eq!(
                seen.channels,
                [
                    channel("other", "dev", 1, "succeeded"),
                    channel("view", "dev", 2, "running"),
                    channel("view", "release", 3, "failed"),
                ]
            );
            let running = |job: &str| MachineJob {
                repo: "view".to_owned(),
                channel: "dev".to_owned(),
                number: 2,
                job: job.to_owned(),
            };
            let machines: Vec<_> = seen
                .machines
                .iter()
                .map(|m| {
                    (
                        m.name.as_str(),
                        m.kind.as_str(),
                        m.status.as_str(),
                        m.jobs.clone(),
                    )
                })
                .collect();
            // Runners by name, offline ones too, then the machines they report.
            assert_eq!(
                machines,
                [
                    ("alpha", "runner", "busy (set)", vec![running("build")]),
                    ("beta", "runner", "offline", vec![running("mac")]),
                    ("win11", "windows-vm", "running", vec![running("windows")]),
                ]
            );

            // A runner that never reported a busy state is just online; one that
            // did and is on the automatic mode shows the measurement alone.
            connect(&server, "beta", None);
            assert_eq!(overview(&server).machines[1].status, "online");
            connect(
                &server,
                "beta",
                Some(MachineState {
                    busy: false,
                    mode: BusyMode::Auto,
                }),
            );
            assert_eq!(overview(&server).machines[1].status, "free");
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_name_that_is_no_repository_reads_nothing() {
            let state = scratch("names");
            store(
                &state,
                &pipeline("view", 1, "dev", PipelineStatus::Succeeded, Vec::new()),
            );
            let server = server(&state);
            // A repository's records, the long way round through the path.
            let sneaky = "../../ci/pipelines/view";
            assert_eq!(
                server.answer(CiRequest::Pipelines {
                    repo: sneaky.to_owned(),
                    channel: "dev".to_owned(),
                    before: None,
                    limit: 10,
                }),
                CiReply::Pipelines {
                    pipelines: Vec::new()
                }
            );
            assert_eq!(
                server.answer(CiRequest::Pipeline {
                    repo: sneaky.to_owned(),
                    number: 1
                }),
                CiReply::Pipeline { pipeline: None }
            );
            let CiReply::Pipeline { pipeline } = server.answer(CiRequest::Pipeline {
                repo: "view".to_owned(),
                number: 1,
            }) else {
                panic!("expected a pipeline reply");
            };
            assert_eq!(pipeline.map(|p| p.number), Some(1));
            assert_eq!(open(&server, sneaky, 1, "j0"), error("no such job"));
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_pipeline_is_found_by_the_prefix_of_its_sha_in_any_channel_newest_first() {
            let state = scratch("find");
            let with_sha = |number, channel: &str, sha: &str| {
                let mut record =
                    pipeline("view", number, channel, PipelineStatus::Running, Vec::new());
                record.sha = Some(sha.to_owned());
                store(&state, &record);
            };
            with_sha(2, "dev", "abcdef0123456789abcdef0123456789abcdef01");
            // A rerun of the same commit, and a manual run of another one.
            with_sha(9, "release", "abcdef0123456789abcdef0123456789abcdef01");
            with_sha(10, "nightly", "1234567890abcdef1234567890abcdef12345678");
            let server = server(&state);
            let find = |sha: &str| match server.answer(CiRequest::Find {
                repo: "view".to_owned(),
                sha: sha.to_owned(),
            }) {
                CiReply::Pipeline { pipeline } => pipeline.map(|p| (p.number, p.channel)),
                other => panic!("unexpected reply {other:?}"),
            };

            assert_eq!(find("abcdef0"), Some((9, "release".to_owned())));
            assert_eq!(find("ABCD"), Some((9, "release".to_owned())));
            assert_eq!(find("1234"), Some((10, "nightly".to_owned())));
            assert_eq!(find("ffff"), None);

            for bad in ["abc", "xyz1", "", &"a".repeat(41)] {
                assert_eq!(
                    server.answer(CiRequest::Find {
                        repo: "view".to_owned(),
                        sha: bad.to_owned(),
                    }),
                    error("sha must be 4 to 40 hex digits"),
                    "{bad:?}"
                );
            }
            assert_eq!(
                server.answer(CiRequest::Find {
                    repo: "../view".to_owned(),
                    sha: "abcd".to_owned(),
                }),
                error("no such repository")
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        /// The origin remote of a checkout of the repository `view`, as
        /// `ci.toml` spells it.
        const VIEW_ORIGIN: &str = "ssh://git@git.example.org/me/view.git";

        /// `ci.toml` tables for the repository `view`, which has a forge, and
        /// `plain`, which has none and a default branch of its own.
        fn repos() -> String {
            format!(
                "[repos.view]\nurl = '{VIEW_ORIGIN}'\nforge = \"forgejo\"\nslug = \"me/view\"\n\
                 api = \"https://git.example.org\"\n\
                 [repos.plain]\nurl = '/srv/git/plain.git'\ndefault_branch = \"trunk\"\n"
            )
        }

        /// What a `Repo` request answers: the view, or `None` for an origin no
        /// repository has.
        fn ask_repo(
            server: &CiServer,
            origin: &str,
            sha: Option<&str>,
            jobs: bool,
        ) -> Option<RepoView> {
            match server.answer(CiRequest::Repo {
                origin: origin.to_owned(),
                sha: sha.map(str::to_owned),
                jobs,
            }) {
                CiReply::Repo { repo } => repo.map(|view| *view),
                other => panic!("unexpected reply {other:?}"),
            }
        }

        /// The view of the repository `view`.
        fn view_of(server: &CiServer, sha: Option<&str>, jobs: bool) -> RepoView {
            ask_repo(server, VIEW_ORIGIN, sha, jobs).expect("the repository is configured")
        }

        #[test]
        fn a_repository_is_found_by_its_origin_in_any_spelling_of_its_url() {
            let state = scratch("repo-origin");
            let server = server_with(&state, &repos());
            let named = |origin: &str| ask_repo(&server, origin, None, false).map(|view| view.name);

            // `ci.toml` has the ssh form; a checkout may have another.
            for origin in [
                VIEW_ORIGIN,
                "git@git.example.org:me/view.git",
                "https://git.example.org/me/view",
                "https://git.example.org/me/view.git/",
            ] {
                assert_eq!(named(origin).as_deref(), Some("view"), "{origin}");
            }
            assert_eq!(named("/srv/git/plain.git").as_deref(), Some("plain"));
            assert_eq!(named("https://git.example.org/me/other"), None);
            assert_eq!(named("https://git.example.org/me"), None);
            assert_eq!(named("/tmp/view"), None);

            // What `ci.toml` says of the repository and of what builds where.
            let view = view_of(&server, None, false);
            assert_eq!(
                (view.forge.as_str(), view.default_branch.as_str()),
                ("forgejo me/view", "main")
            );
            let plain = ask_repo(&server, "/srv/git/plain.git", None, false).expect("plain");
            assert_eq!(
                (plain.forge.as_str(), plain.default_branch.as_str()),
                ("none", "trunk")
            );
            assert_eq!(
                view.platforms,
                [
                    ("linux-amd64".to_owned(), "alpha".to_owned()),
                    ("windows-amd64".to_owned(), "win11".to_owned()),
                ]
            );

            // The runners are those of `ci.toml`, with the state they report.
            let runners = |server: &CiServer| {
                let view = view_of(server, None, false);
                view.machines
                    .into_iter()
                    .map(|machine| (machine.name, machine.status))
                    .collect::<Vec<_>>()
            };
            let pair = |name: &str, status: &str| (name.to_owned(), status.to_owned());
            assert_eq!(
                runners(&server),
                [pair("alpha", "offline"), pair("beta", "offline")]
            );
            connect(&server, "alpha", None);
            assert_eq!(
                runners(&server),
                [pair("alpha", "online"), pair("beta", "offline")]
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_repo_request_names_an_origin_and_a_sha_within_bounds() {
            let state = scratch("repo-bounds");
            let server = server_with(&state, &repos());
            let reply = |origin: &str, sha: Option<&str>| {
                server.answer(CiRequest::Repo {
                    origin: origin.to_owned(),
                    sha: sha.map(str::to_owned),
                    jobs: false,
                })
            };

            for origin in [String::new(), "a".repeat(MAX_ORIGIN_BYTES + 1)] {
                assert_eq!(
                    reply(&origin, None),
                    error(&format!("origin must be 1 to {MAX_ORIGIN_BYTES} bytes")),
                    "{} bytes",
                    origin.len()
                );
            }
            // The longest origin is looked up, and no repository has it.
            assert_eq!(
                reply(&"a".repeat(MAX_ORIGIN_BYTES), None),
                CiReply::Repo { repo: None }
            );

            for bad in ["abc", "xyz1", "", &"a".repeat(41)] {
                assert_eq!(
                    reply(VIEW_ORIGIN, Some(bad)),
                    error("sha must be 4 to 40 hex digits"),
                    "{bad:?}"
                );
            }
            for good in ["abcd", "ABCDEF0", &"0".repeat(40)] {
                assert!(
                    matches!(
                        reply(VIEW_ORIGIN, Some(good)),
                        CiReply::Repo { repo: Some(_) }
                    ),
                    "{good:?}"
                );
            }
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn the_view_has_the_newest_pipeline_of_the_sha_and_the_channels_of_its_repository_only() {
            let state = scratch("repo-head");
            let with_sha = |repo: &str, number, channel: &str, sha: &str| {
                let mut record =
                    pipeline(repo, number, channel, PipelineStatus::Succeeded, Vec::new());
                record.sha = Some(sha.to_owned());
                store(&state, &record);
            };
            with_sha("view", 2, "dev", "abcdef0123456789abcdef0123456789abcdef01");
            // A rerun of the same commit, another commit, and the first one in
            // another repository.
            with_sha(
                "view",
                9,
                "release",
                "abcdef0123456789abcdef0123456789abcdef01",
            );
            with_sha(
                "view",
                10,
                "nightly",
                "1234567890abcdef1234567890abcdef12345678",
            );
            with_sha(
                "plain",
                11,
                "dev",
                "abcdef0123456789abcdef0123456789abcdef01",
            );
            let server = server_with(&state, &repos());
            let head = |sha: Option<&str>| {
                view_of(&server, sha, false)
                    .head
                    .map(|head| (head.repo, head.number, head.channel))
            };

            let rerun = Some(("view".to_owned(), 9, "release".to_owned()));
            assert_eq!(head(Some("abcdef0")), rerun);
            assert_eq!(
                head(Some("ABCDEF0123456789ABCDEF0123456789ABCDEF01")),
                rerun
            );
            assert_eq!(
                head(Some("1234")),
                Some(("view".to_owned(), 10, "nightly".to_owned()))
            );
            assert_eq!(head(Some("ffff")), None);
            assert_eq!(head(None), None);

            let channel = |channel: &str, latest| ChannelView {
                repo: "view".to_owned(),
                channel: channel.to_owned(),
                latest,
                status: "succeeded".to_owned(),
            };
            assert_eq!(
                view_of(&server, None, false).channels,
                [
                    channel("dev", 2),
                    channel("nightly", 10),
                    channel("release", 9)
                ]
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn the_default_branch_is_green_or_red_since_the_pipeline_that_broke_it() {
            let state = scratch("repo-branch");
            let server = server_with(&state, &repos());
            // A run of `repo` on `branch` whose `check` job ended as `check`.
            // Both jobs have a pattern of their own, so neither is a deploy
            // job, which the streak would also name.
            let run = |repo: &str, number: u64, branch: &str, check: JobStatus| {
                let mut jobs = vec![
                    job("build", JobStatus::Succeeded, None, None),
                    job("check", check, None, None),
                ];
                for job in &mut jobs {
                    job.def.on = vec!["push main".to_owned()];
                }
                let mut record = pipeline(repo, number, "dev", PipelineStatus::Failed, jobs);
                record.event.git_ref = branch.to_owned();
                record.sha = Some(format!("{number}{}", "a".repeat(39)));
                record.created = crate::now_secs();
                store(&state, &record);
            };
            let branch = |server: &CiServer| view_of(server, None, false).branch;

            // Nothing has run yet.
            assert_eq!(branch(&server), "green");
            run("view", 1, "main", JobStatus::Succeeded);
            assert_eq!(branch(&server), "green");
            run("view", 2, "main", JobStatus::Failed);
            run("view", 3, "main", JobStatus::Failed);
            assert_eq!(
                branch(&server),
                "check red since #2 (2aaaaaa, 0 min, 2 pipelines)"
            );
            // Another branch breaking says nothing about this one.
            run("view", 4, "feature/x", JobStatus::Succeeded);
            assert_eq!(
                branch(&server),
                "check red since #2 (2aaaaaa, 0 min, 2 pipelines)"
            );
            run("view", 5, "main", JobStatus::Succeeded);
            assert_eq!(branch(&server), "green");

            // The default branch is the one `ci.toml` names: `plain` has `trunk`.
            run("plain", 1, "main", JobStatus::Failed);
            let plain = |server: &CiServer| {
                ask_repo(server, "/srv/git/plain.git", None, false)
                    .expect("plain")
                    .branch
            };
            assert_eq!(plain(&server), "green");
            run("plain", 2, "trunk", JobStatus::Failed);
            assert_eq!(
                plain(&server),
                "check red since #2 (2aaaaaa, 0 min, 1 pipeline)"
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        const BUILD: (&str, &str) = (
            ".zeughaus-ci/build.sh",
            "# /// ci\n# on = [\"push main\", \"tag v*\"]\n# ///\necho build\n",
        );

        const CHECK: (&str, &str) = (
            ".zeughaus-ci/check.sh",
            "# /// ci\n# needs = [\"build\"]\n# image = \"trixie\"\n# when_busy = \"freeze\"\n# ///\necho check\n",
        );

        const TRIXIE: (&str, &str) = (".zeughaus-ci/trixie.Containerfile", "FROM scratch\n");

        const PUBLISH: (&str, &str) = (
            ".zeughaus-ci/publish.sh",
            "# /// ci\n# needs = [\"check\"]\n# channels = [\"dev\"]\n# secrets = [\"TOKEN\", \"OTHER_TOKEN\"]\n# ///\necho publish\n",
        );

        const LINUX: (&str, &str) = (
            ".zeughaus-ci/linux.sh",
            "# /// ci\n# on = [\"push main\"]\n# platform = \"linux-amd64\"\n# ///\necho linux\n",
        );

        const MAC: (&str, &str) = (
            ".zeughaus-ci/mac.sh",
            "# /// ci\n# on = [\"push main\"]\n# platform = \"darwin-arm64\"\n# ///\necho mac\n",
        );

        const WINDOWS: (&str, &str) = (
            ".zeughaus-ci/windows.ps1",
            "# /// ci\n# on = [\"push main\"]\n# machine = \"win11\"\n# ///\nWrite-Output windows\n",
        );

        const WINDOWS_AMD64: (&str, &str) = (
            ".zeughaus-ci/windows-amd64.ps1",
            "# /// ci\n# on = [\"push main\"]\n# platform = \"windows-amd64\"\n# ///\nWrite-Output windows\n",
        );

        const CHANNELS: (&str, &str) = (
            ".zeughaus-ci/channels.toml",
            "[[channel]]\nname = \"dev\"\non = [\"push main\"]\n",
        );

        /// The scheduler's mirror of the repository `view`, a bare repository
        /// with the branch `main` of a work tree that committed `commits`,
        /// oldest first: a commit's `.zeughaus-ci/` holds the files it lists.
        /// Returns the commit ids. No commit leaves a mirror with nothing in it.
        fn mirror_with(state: &Path, commits: &[&[(&str, &str)]]) -> Vec<String> {
            let author: Vec<(String, String)> = [
                ("GIT_AUTHOR_NAME", "t"),
                ("GIT_AUTHOR_EMAIL", "t@t"),
                ("GIT_COMMITTER_NAME", "t"),
                ("GIT_COMMITTER_EMAIL", "t@t"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
            let git = |dir: &Path, args: &[&str]| {
                scheduler::git(dir, args, &author).unwrap_or_else(|e| panic!("{e}"))
            };
            let work = state.join("work");
            std::fs::create_dir_all(&work).expect("create the work tree");
            git(&work, &["init", "-q", "-b", "main"]);
            let mut ids = Vec::new();
            for files in commits {
                let _ = std::fs::remove_dir_all(work.join(CI_DIR));
                for (path, text) in *files {
                    let path = work.join(path);
                    std::fs::create_dir_all(path.parent().expect("a file in a folder"))
                        .expect("create the folder");
                    std::fs::write(&path, text).expect("write the file");
                }
                git(&work, &["add", "-A"]);
                git(&work, &["commit", "-q", "--allow-empty", "-m", "snapshot"]);
                ids.push(git(&work, &["rev-parse", "HEAD"]).trim().to_owned());
            }
            let mirror = scheduler::mirror_dir(state, "view");
            std::fs::create_dir_all(&mirror).expect("create the mirror");
            git(&mirror, &["init", "--bare", "-q", "-b", "main"]);
            if !ids.is_empty() {
                let from = work.to_string_lossy();
                git(
                    &mirror,
                    &["fetch", "-q", &from, "+refs/heads/*:refs/heads/*"],
                );
            }
            ids
        }

        #[test]
        fn the_jobs_are_read_at_the_asked_commit_else_at_the_head_of_the_default_branch() {
            let state = scratch("repo-jobs-at");
            let ids = mirror_with(
                &state,
                &[
                    &[BUILD, CHECK, TRIXIE, CHANNELS],
                    &[BUILD, CHECK, TRIXIE, CHANNELS, PUBLISH, LINUX],
                ],
            );
            let server = server_with(&state, &repos());
            let read = |sha: Option<&str>| {
                let view = view_of(&server, sha, true);
                let mut names: Vec<&str> = view.jobs.iter().map(|job| job.name.as_str()).collect();
                names.sort_unstable();
                (
                    view.jobs_sha.clone(),
                    names.join(" "),
                    view.jobs_note.clone(),
                )
            };
            let old = (
                Some(ids[0].clone()),
                "build check".to_owned(),
                String::new(),
            );
            let head = (
                Some(ids[1].clone()),
                "build check linux publish".to_owned(),
                String::new(),
            );

            // The commit asked for, whole or by its start.
            assert_eq!(read(Some(&ids[0])), old);
            assert_eq!(read(Some(&ids[0][..8])), old);
            assert_eq!(read(Some(&ids[0][..8].to_uppercase())), old);
            assert_eq!(read(Some(&ids[1])), head);
            // One the mirror does not have, or none: the head of `main`.
            assert_eq!(read(Some(&"f".repeat(40))), head);
            assert_eq!(read(None), head);

            // Without the jobs asked for, git is not run and nothing is said.
            let bare = view_of(&server, Some(&ids[0]), false);
            assert_eq!(
                (bare.jobs_sha, bare.jobs.len(), bare.jobs_note),
                (None, 0, String::new())
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn each_job_says_where_it_runs_what_it_needs_and_whether_run_jobs_takes_it() {
            let state = scratch("repo-jobs-place");
            mirror_with(
                &state,
                &[&[
                    BUILD,
                    CHECK,
                    TRIXIE,
                    PUBLISH,
                    LINUX,
                    MAC,
                    WINDOWS,
                    WINDOWS_AMD64,
                    CHANNELS,
                ]],
            );
            let server = server_with(&state, &repos());
            let view = view_of(&server, None, true);
            assert_eq!(view.jobs_note, "");
            let job = |name: &str| {
                view.jobs
                    .iter()
                    .find(|job| job.name == name)
                    .unwrap_or_else(|| panic!("no job {name}"))
            };
            // The runner, where on it, and whether a manual run takes the job.
            let placed = |name: &str| {
                let job = job(name);
                (job.runner.as_str(), job.at.as_str(), job.manual)
            };
            assert_eq!(placed("build"), ("alpha", "host", true));
            assert_eq!(placed("check"), ("alpha", "image:trixie", true));
            assert_eq!(placed("publish"), ("alpha", "host", true));
            assert_eq!(placed("linux"), ("alpha", "host", true));
            // A machine is a Windows VM, which a manual run does not take.
            assert_eq!(placed("windows"), ("alpha", "machine:win11", false));
            assert_eq!(placed("windows-amd64"), ("alpha", "machine:win11", false));
            // A platform `ci.toml` has no entry for runs nowhere.
            assert_eq!(placed("mac"), ("", "", false));
            assert_eq!(
                job("mac").placement,
                ".zeughaus-ci/mac.sh: platform darwin-arm64 is not in ci.toml [platforms]"
            );
            let unplaced: Vec<&str> = view
                .jobs
                .iter()
                .filter(|job| !job.placement.is_empty())
                .map(|job| job.name.as_str())
                .collect();
            assert_eq!(unplaced, ["mac"]);

            // The header as written.
            let build = job("build");
            assert_eq!(
                (build.file.as_str(), build.place.as_str()),
                ("build.sh", "host")
            );
            assert_eq!(build.on, ["push main", "tag v*"]);
            assert_eq!(build.when_busy, "wait");
            assert!(build.needs.is_empty() && build.secrets.is_empty());
            let check = job("check");
            assert_eq!(check.place, "image:trixie");
            assert_eq!(
                (check.needs.as_slice(), check.when_busy.as_str()),
                (&["build".to_owned()][..], "freeze")
            );
            let publish = job("publish");
            assert_eq!(publish.secrets, ["TOKEN", "OTHER_TOKEN"]);
            assert_eq!(
                (publish.needs.as_slice(), publish.channels.as_slice()),
                (&["check".to_owned()][..], &["dev".to_owned()][..])
            );
            assert!(publish.on.is_empty());
            assert_eq!(job("linux").place, "platform:linux-amd64");
            assert_eq!(job("windows").place, "machine:win11");
            assert_eq!(job("windows-amd64").place, "platform:windows-amd64");
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_repository_the_ci_has_not_fetched_has_no_jobs_to_show_and_says_why() {
            let state = scratch("repo-jobs-unfetched");
            let server = server_with(&state, &repos());
            let view = view_of(&server, Some(&"a".repeat(40)), true);
            assert_eq!(
                (view.jobs_sha, view.jobs.len(), view.jobs_note.as_str()),
                (
                    None,
                    0,
                    "the CI has not fetched view yet; it does with its first event"
                )
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn jobs_that_cannot_be_read_are_replaced_by_the_reason() {
            let state = scratch("repo-jobs-note");
            let server = server_with(&state, &repos());
            let note = |sha: Option<&str>| {
                let view = view_of(&server, sha, true);
                assert!(view.jobs.is_empty());
                (view.jobs_sha, view.jobs_note)
            };

            // A mirror that is set up and has no commit yet.
            assert!(mirror_with(&state, &[]).is_empty());
            assert_eq!(
                note(None),
                (
                    None,
                    "no commit of view to read .zeughaus-ci/ from".to_owned()
                )
            );

            let ids = mirror_with(
                &state,
                &[
                    &[("README", "view\n")],
                    &[(
                        ".zeughaus-ci/bad.sh",
                        "# /// ci\n# nonsense = 1\n# ///\necho\n",
                    )],
                ],
            );
            // No folder, then a header the CI cannot read.
            assert_eq!(
                note(Some(&ids[0])),
                (
                    Some(ids[0].clone()),
                    format!("{} has no job in .zeughaus-ci/", &ids[0][..7])
                )
            );
            let (sha, text) = note(Some(&ids[1]));
            assert_eq!(sha, Some(ids[1].clone()));
            let reason = format!("{} .zeughaus-ci/: .zeughaus-ci/bad.sh: ", &ids[1][..7]);
            assert!(text.starts_with(&reason), "{text}");
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_transcript_opens_for_a_job_that_has_output_or_is_running() {
            let state = scratch("open");
            store(
                &state,
                &pipeline(
                    "view",
                    1,
                    "dev",
                    PipelineStatus::Running,
                    vec![
                        job("done", JobStatus::Succeeded, Some("alpha"), None),
                        job("live", JobStatus::Running, Some("alpha"), None),
                        job("waiting", JobStatus::Pending, None, None),
                        job("empty", JobStatus::Failed, Some("alpha"), None),
                    ],
                ),
            );
            let kept = transcript_dir(&state, "view", 1, "done");
            std::fs::create_dir_all(&kept).expect("create the store");
            std::fs::write(kept.join("wal"), "").expect("write the kept WAL");
            let server = server(&state);

            let CiReply::Transcript { terminal } = open(&server, "view", 1, "done") else {
                panic!("a job with a stored WAL opens");
            };
            // The same job is the same terminal.
            assert_eq!(
                open(&server, "view", 1, "done"),
                CiReply::Transcript { terminal }
            );
            assert!(matches!(
                open(&server, "view", 1, "live"),
                CiReply::Transcript { .. }
            ));
            assert_eq!(
                open(&server, "view", 1, "waiting"),
                error("waiting has no transcript")
            );
            assert_eq!(
                open(&server, "view", 1, "empty"),
                error("empty has no transcript")
            );
            assert_eq!(open(&server, "view", 1, "nope"), error("no such job"));
            assert_eq!(open(&server, "view", 9, "done"), error("no such job"));

            // Closing is the editor's, and idempotent.
            for _ in 0..2 {
                assert_eq!(
                    server.answer(CiRequest::CloseTranscript { terminal }),
                    CiReply::Closed
                );
            }
            assert!(!server.transcripts.exists(TerminalId(terminal)));
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_rerun_queues_the_same_event_pinned_to_the_recorded_sha() {
            let state = scratch("rerun");
            let mut record = pipeline("view", 4, "dev", PipelineStatus::Failed, Vec::new());
            record.event.delivery = Some("abc".to_owned());
            store(&state, &record);
            let server = server(&state);

            assert_eq!(
                server.answer(CiRequest::Rerun {
                    repo: "view".to_owned(),
                    number: 4,
                }),
                CiReply::Accepted
            );
            event::drain_inbox(&state);
            let queued = event::queued(&state);
            assert_eq!(queued.len(), 1);
            let queued = &queued[0].1;
            assert_eq!(queued.sha, record.sha);
            assert_eq!(queued.actor, "rerun of #4");
            assert_eq!(queued.delivery, None);
            assert_eq!(
                (queued.kind, queued.git_ref.as_str(), queued.cron.as_ref()),
                (record.event.kind, "main", None)
            );

            let unknown = |repo: &str, number| {
                server.answer(CiRequest::Rerun {
                    repo: repo.to_owned(),
                    number,
                })
            };
            assert_eq!(unknown("view", 9), error("no such pipeline"));
            assert_eq!(unknown("../view", 4), error("no such pipeline"));
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_cancel_is_left_for_the_scheduler_when_the_pipeline_runs() {
            let state = scratch("cancel");
            store(
                &state,
                &pipeline("view", 1, "dev", PipelineStatus::Running, Vec::new()),
            );
            store(
                &state,
                &pipeline("view", 2, "dev", PipelineStatus::Succeeded, Vec::new()),
            );
            let server = server(&state);
            let cancel = |number| {
                server.answer(CiRequest::Cancel {
                    repo: "view".to_owned(),
                    number,
                })
            };

            assert_eq!(cancel(1), CiReply::Accepted);
            assert_eq!(event::drain_cancels(&state), [("view".to_owned(), 1)]);
            assert_eq!(cancel(2), error("the pipeline is not running"));
            assert_eq!(cancel(3), error("no such pipeline"));
            assert!(event::drain_cancels(&state).is_empty());
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_manual_run_cannot_be_rerun() {
            let state = scratch("rerun-manual");
            let mut record = pipeline("view", 4, "dev", PipelineStatus::Failed, Vec::new());
            record.event.kind = EventKind::Manual;
            record.event.git_ref = "main".to_owned();
            record.event.upload = Some("0123456789abcdef".to_owned());
            store(&state, &record);
            let server = server(&state);
            assert_eq!(
                server.answer(CiRequest::Rerun {
                    repo: "view".to_owned(),
                    number: 4,
                }),
                error("a manual run cannot be rerun; start a new one")
            );
            event::drain_inbox(&state);
            assert!(event::queued(&state).is_empty());
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_jobs_log_is_its_last_lines_and_says_whether_it_ended() {
            let state = scratch("log");
            let dir = transcript_dir(&state, "view", 3, "build");
            std::fs::create_dir_all(&dir).expect("create the transcript directory");
            let server = server(&state);
            let ask = |repo: &str, job: &str, tail, lines: Vec<LineSpan>| {
                server.answer(CiRequest::Log {
                    repo: repo.to_owned(),
                    number: 3,
                    job: job.to_owned(),
                    tail,
                    timestamps: false,
                    lines,
                })
            };
            let log = |tail, lines| match ask("view", "build", tail, lines) {
                CiReply::Log { text, finished } => (text, finished),
                other => panic!("unexpected reply {other:?}"),
            };

            assert_eq!(
                ask("view", "build", 10, Vec::new()),
                error("no output for build of #3")
            );
            let mut wal = zeughaus_terminal::wal::WalWriter::open(&dir.join("wal"), None)
                .expect("open the WAL");
            wal.append(1, b"\x1b[32mone\x1b[0m\r\ntwo\r\n")
                .expect("append");
            wal.append(2, b"three\r\nfour\r\n").expect("append");

            // The escape sequences are gone, only the end is asked for, every
            // line has its number, and the job still runs.
            assert_eq!(
                log(2, Vec::new()),
                ("3| three\n4| four\n".to_owned(), false)
            );
            assert_eq!(
                log(100, Vec::new()),
                ("1| one\n2| two\n3| three\n4| four\n".to_owned(), false)
            );
            // Lines asked for by number replace the tail.
            let middle = vec![LineSpan::Range {
                first: 2,
                last: Some(3),
            }];
            assert_eq!(log(1, middle), ("2| two\n3| three\n".to_owned(), false));
            std::fs::write(dir.join("exit"), "code=0\n").expect("write the exit record");
            assert_eq!(log(1, Vec::new()), ("4| four\n".to_owned(), true));

            // Names that are no job read nothing.
            assert_eq!(
                ask("../view", "build", 10, Vec::new()),
                error("no such job")
            );
            assert_eq!(
                ask("view", "../build", 10, Vec::new()),
                error("no such job")
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_jobs_log_can_start_every_line_with_the_time_it_began() {
            let state = scratch("log-times");
            let dir = transcript_dir(&state, "view", 3, "build");
            std::fs::create_dir_all(&dir).expect("create the transcript directory");
            let server = server(&state);
            let log = |tail| match server.answer(CiRequest::Log {
                repo: "view".to_owned(),
                number: 3,
                job: "build".to_owned(),
                tail,
                timestamps: true,
                lines: Vec::new(),
            }) {
                CiReply::Log { text, finished } => (text, finished),
                other => panic!("unexpected reply {other:?}"),
            };
            let mut writer = wal::WalWriter::open(&dir.join("wal"), None).expect("open the WAL");
            // The first output is the origin. "slow" begins in the first chunk
            // and ends in the second, a blank line has no text to stamp, and
            // the last line has no line feed yet.
            writer
                .append(10_000_000, b"\x1b[32mone\x1b[0m\r\nsl")
                .expect("append");
            writer
                .append(11_500_000, b"ow\r\n\r\ntwo\r\n")
                .expect("append");
            writer.append(75_800_000, b"three").expect("append");

            assert_eq!(
                log(100),
                (
                    "1| [+     0.0s] one\n2| [+     0.0s] slow\n3| [+     1.5s] two\n\
                     4| [+    65.8s] three\n"
                        .to_owned(),
                    false
                )
            );
            assert_eq!(
                log(2),
                (
                    "3| [+     1.5s] two\n4| [+    65.8s] three\n".to_owned(),
                    false
                )
            );
            std::fs::write(dir.join("exit"), "code=0\n").expect("write the exit record");
            assert_eq!(log(1), ("4| [+    65.8s] three\n".to_owned(), true));
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_jobs_timing_names_the_lines_the_longest_pauses_follow() {
            let state = scratch("timing");
            let mut build = job("build", JobStatus::Succeeded, Some("alpha"), None);
            // The CI's record of the end: 140 s past the epoch.
            build.finished = Some(140);
            let idle = job("idle", JobStatus::Pending, None, None);
            store(
                &state,
                &pipeline(
                    "view",
                    3,
                    "dev",
                    PipelineStatus::Succeeded,
                    vec![build.clone(), idle],
                ),
            );
            let dir = transcript_dir(&state, "view", 3, "build");
            std::fs::create_dir_all(&dir).expect("create the transcript directory");
            let server = server(&state);
            let ask = |repo: &str, number, job: &str, top| {
                server.answer(CiRequest::Timing {
                    repo: repo.to_owned(),
                    number,
                    job: job.to_owned(),
                    top,
                })
            };
            let timing = |top| match ask("view", 3, "build", top) {
                CiReply::Timing { timing } => timing,
                other => panic!("unexpected reply {other:?}"),
            };

            assert_eq!(
                ask("view", 3, "build", 5),
                error("no output for build of #3")
            );
            let mut writer = wal::WalWriter::open(&dir.join("wal"), None).expect("open the WAL");
            writer.append(100_000_000, b"start\r\n").expect("append");
            writer
                .append(100_200_000, b"compiling a\r\n")
                .expect("append");
            writer
                .append(130_200_000, b"compiling b\r\n")
                .expect("append");
            writer.append(131_000_000, b"link\r\n").expect("append");

            // The pause after "compiling a" is 30 s and the one after "link"
            // runs to the end of the job, 9 s; times are from the first output.
            let two = timing(2);
            assert_eq!((two.lines, two.span_ms), (4, 40_000));
            assert_eq!(
                two.slowest,
                vec![
                    TimedLine {
                        line: 2,
                        at_ms: 200,
                        took_ms: 30_000,
                        text: "compiling a".to_owned()
                    },
                    TimedLine {
                        line: 4,
                        at_ms: 31_000,
                        took_ms: 9_000,
                        text: "link".to_owned()
                    },
                ]
            );
            assert!(!two.finished && !two.truncated);
            // The number of lines asked for is kept within what a reply allows.
            assert_eq!(timing(0).slowest.len(), 1);
            assert_eq!(timing(1000).slowest.len(), 4);
            std::fs::write(dir.join("exit"), "code=0\n").expect("write the exit record");
            assert!(timing(1).finished);

            // A job the record has no end for is still running: its last line
            // has stood until now.
            let mut running = build;
            running.finished = None;
            store(
                &state,
                &pipeline("view", 3, "dev", PipelineStatus::Running, vec![running]),
            );
            let now = timing(1);
            assert!(now.span_ms > 1_000_000_000, "{now:?}");

            // No such pipeline, job or name; a job without output.
            assert_eq!(ask("view", 9, "build", 5), error("no such job"));
            assert_eq!(ask("view", 3, "nope", 5), error("no such job"));
            assert_eq!(ask("view", 3, "../build", 5), error("no such job"));
            assert_eq!(ask("../view", 3, "build", 5), error("no such job"));
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_job_without_output_in_its_record_has_nothing_to_time() {
            let state = scratch("timing-idle");
            store(
                &state,
                &pipeline(
                    "view",
                    3,
                    "dev",
                    PipelineStatus::Running,
                    vec![job("idle", JobStatus::Pending, None, None)],
                ),
            );
            let server = server(&state);
            assert_eq!(
                server.answer(CiRequest::Timing {
                    repo: "view".to_owned(),
                    number: 3,
                    job: "idle".to_owned(),
                    top: 5,
                }),
                error("no output for idle of #3")
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_window_that_opens_inside_a_line_drops_the_rest_of_it() {
            let state = scratch("window");
            let dir = transcript_dir(&state, "view", 3, "build");
            std::fs::create_dir_all(&dir).expect("create the transcript directory");
            let mut writer = wal::WalWriter::open(&dir.join("wal"), None).expect("open the WAL");
            writer.append(1_000_000, b"alpha\nbe").expect("append");
            writer.append(2_000_000, b"ta\ngamma\n").expect("append");
            writer.append(3_000_000, b"delta\n").expect("append");
            let line = |at, text: &str| (at, text.to_owned());

            let all = timed_tail(&dir, 1 << 20).expect("the whole log");
            assert!(!all.truncated);
            assert_eq!(all.origin, Some(1_000_000));
            assert_eq!(
                all.lines,
                [
                    line(1_000_000, "alpha"),
                    line(1_000_000, "beta"),
                    line(2_000_000, "gamma"),
                    line(3_000_000, "delta"),
                ]
            );

            // Seven bytes need the second record whole, and its first line is
            // the end of "beta", which began in the record left out. The origin
            // is still the log's first output.
            let tail = timed_tail(&dir, 7).expect("the last records");
            assert!(tail.truncated);
            assert_eq!(tail.origin, Some(1_000_000));
            assert_eq!(
                tail.lines,
                [line(2_000_000, "gamma"), line(3_000_000, "delta")]
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn a_stored_document_is_counted_listed_and_announced() {
            let state = scratch("docs");
            let (tx, rx) = std::sync::mpsc::channel();
            let server = CiServer::new(
                state.clone(),
                config("[repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\n"),
                Arc::new(Runners::new()),
                Arc::new(Transcripts::new(state.clone())),
                tx,
            );
            let made = server.answer(CiRequest::DocCreate {
                repo: "demo".to_owned(),
                kind: zeughaus_link::DocKind::Plan,
                title: "  Smoke plan ".to_owned(),
                body: "# Smoke plan\n".to_owned(),
                source: "demo".to_owned(),
                proposals: Vec::new(),
            });
            assert_eq!(made, CiReply::DocCreated { number: 1 });
            assert_eq!(rx.try_recv(), Ok("demo".to_owned()));

            let CiReply::Overview { overview } = server.answer(CiRequest::Overview) else {
                panic!("no overview");
            };
            assert_eq!(
                overview.repos,
                vec![zeughaus_link::RepoDocsView {
                    repo: "demo".to_owned(),
                    origin: "git.example.org/o/demo".to_owned(),
                    default_branch: "main".to_owned(),
                    awaiting: 1,
                    queued: 0,
                    running: None,
                    proposals: 0,
                }]
            );
            let CiReply::Docs { docs } = server.answer(CiRequest::Docs {
                repo: "demo".to_owned(),
            }) else {
                panic!("no docs");
            };
            assert_eq!(docs.len(), 1);
            assert_eq!(docs[0].title, "Smoke plan");
            assert_eq!(docs[0].state, DocState::Saved);
            assert!(docs[0].body.is_empty());

            // A claim that finds nothing approved changes nothing to announce.
            let none = server.answer(CiRequest::PlanClaim {
                repo: "demo".to_owned(),
            });
            assert_eq!(none, CiReply::Doc { doc: None });
            assert!(rx.try_recv().is_err());
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}
