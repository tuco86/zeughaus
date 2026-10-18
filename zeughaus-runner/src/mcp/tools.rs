//! The nine tools, the schemas an agent sees and the text they answer with.
//!
//! The text is for a model, so it is short, one fact per line and in a shape
//! that is the same every time. The functions that build it take the records
//! and the clock as arguments and touch no network.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use zeughaus_link::ci::{MAX_PIPELINES_PER_REQUEST, MAX_TIMING_LINES, TIMING_READ_BYTES};
use zeughaus_link::manual::{MAX_MANUAL_JOBS, RunReply};
use zeughaus_link::{
    CiOverview, CiReply, CiRequest, JobTiming, JobView, PipelineView, TransferView,
};

use super::client::Client;
use super::rpc::Tools;
use super::snapshot;
use crate::names::valid_name;

const INSTRUCTIONS: &str = "zeughaus is this person's CI: its runners are the machines they own. \
run_jobs runs named jobs of the repository's pipeline (for example build-windows-amd64 and \
check-windows-amd64) from the current working tree, uncommitted files and sibling path \
dependencies included, in the same warm workspace the pipeline uses at that job's place, so the \
build is incremental; it is the way to build, test or debug on another platform. ci_wait waits \
for a pipeline or one job, also for the pipeline a push is about to start; ci_status, \
ci_pipelines, ci_pipeline and ci_log show the CI's state and output; ci_timing says where the \
time went.";

/// Pipelines listed when the call gives no limit.
const DEFAULT_PIPELINES: u32 = 10;
/// Lines of a job's output returned when the call gives no count.
const DEFAULT_TAIL: u32 = 200;
/// The most lines of output one call returns; the CI cuts there too.
const MAX_TAIL: u32 = 5000;
/// Lines `ci_timing` names when the call gives no count.
const DEFAULT_TOP: u32 = 15;
/// How long a call waits for a manual run when it is not told.
const DEFAULT_WAIT_SECONDS: u64 = 540;
/// The most a call waits: the client's own timeout for a tool call is 660 s.
const MAX_WAIT_SECONDS: u64 = 600;
/// Pause between two looks at a running pipeline.
const POLL: Duration = Duration::from_secs(2);
/// How long a wait keeps looking at a CI that does not answer before it
/// gives up on it: a restart or a dropped link is over by then.
const ERROR_GRACE: Duration = Duration::from_secs(60);

pub struct Toolbox {
    client: Arc<Client>,
    poll: Duration,
    error_grace: Duration,
}

impl Toolbox {
    pub fn new(client: Arc<Client>) -> Toolbox {
        Toolbox {
            client,
            poll: POLL,
            error_grace: ERROR_GRACE,
        }
    }

    /// The pause between two looks at a running pipeline, for a test that
    /// cannot wait two seconds per look.
    #[cfg(test)]
    fn polling_every(mut self, poll: Duration) -> Toolbox {
        self.poll = poll;
        self
    }

    /// How long a wait puts up with a CI that does not answer.
    #[cfg(test)]
    fn tolerating_errors_for(mut self, grace: Duration) -> Toolbox {
        self.error_grace = grace;
        self
    }

    async fn status(&self) -> Result<String, String> {
        match self.client.ci(CiRequest::Overview).await? {
            CiReply::Overview { overview } => Ok(status_text(&overview, now())),
            other => Err(unexpected(&other)),
        }
    }

    async fn pipelines(&self, args: PipelinesArgs) -> Result<String, String> {
        let limit = args
            .limit
            .unwrap_or(DEFAULT_PIPELINES)
            .clamp(1, MAX_PIPELINES_PER_REQUEST);
        let request = CiRequest::Pipelines {
            repo: args.repo.clone(),
            channel: args.channel.clone(),
            before: None,
            limit,
        };
        match self.client.ci(request).await? {
            CiReply::Pipelines { pipelines } if pipelines.is_empty() => {
                Ok(format!("no pipelines in {}/{}", args.repo, args.channel))
            }
            CiReply::Pipelines { pipelines } => Ok(pipelines
                .iter()
                .map(|pipeline| pipeline_line(pipeline) + "\n")
                .collect()),
            other => Err(unexpected(&other)),
        }
    }

    async fn pipeline(&self, args: PipelineArgs) -> Result<String, String> {
        let view = self.view(&args.repo, args.number).await?;
        Ok(pipeline_text(&view))
    }

    async fn log(&self, args: LogArgs) -> Result<String, String> {
        let request = CiRequest::Log {
            repo: args.repo,
            number: args.number,
            job: args.job,
            tail: args.tail.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL),
            timestamps: args.timestamps.unwrap_or(false),
        };
        match self.client.ci(request).await? {
            CiReply::Log { text, finished } => Ok(log_text(&text, finished)),
            other => Err(unexpected(&other)),
        }
    }

    /// Where a pipeline's time went; with a job, where its output's did.
    async fn timing(&self, args: TimingArgs) -> Result<String, String> {
        let TimingArgs {
            repo,
            number,
            job,
            top,
        } = args;
        let Some(job) = job else {
            return Ok(timing_text(&self.view(&repo, number).await?));
        };
        let request = CiRequest::Timing {
            repo: repo.clone(),
            number,
            job: job.clone(),
            top: top.unwrap_or(DEFAULT_TOP).clamp(1, MAX_TIMING_LINES),
        };
        match self.client.ci(request).await? {
            CiReply::Timing { timing } => Ok(job_timing_text(&repo, number, &job, &timing)),
            other => Err(unexpected(&other)),
        }
    }

    async fn rerun(&self, args: PipelineArgs) -> Result<String, String> {
        let request = CiRequest::Rerun {
            repo: args.repo,
            number: args.number,
        };
        accepted(self.client.ci(request).await?)
    }

    async fn cancel(&self, args: PipelineArgs) -> Result<String, String> {
        let request = CiRequest::Cancel {
            repo: args.repo,
            number: args.number,
        };
        accepted(self.client.ci(request).await?)
    }

    /// Snapshots the working tree, uploads it with the names of the jobs to
    /// run and, once the CI has numbered the pipeline, waits for it as
    /// `ci_wait` does with what is left of the call's time.
    async fn run_jobs(&self, args: RunJobsArgs) -> Result<String, String> {
        let started = Instant::now();
        if !(1..=MAX_MANUAL_JOBS).contains(&args.jobs.len()) {
            return Err(format!(
                "jobs has {} entries; it must have 1 to {MAX_MANUAL_JOBS}",
                args.jobs.len()
            ));
        }
        for (index, name) in args.jobs.iter().enumerate() {
            if !valid_name(name) {
                return Err(format!(
                    "{name:?} is not a job name: the file of .zeughaus-ci/ without its \
                     extension, lowercase letters, digits and -"
                ));
            }
            if args.jobs[..index].contains(name) {
                return Err(format!("job {name} is named twice"));
            }
        }
        let wait = wait_for(args.wait_seconds);
        // The snapshot and its upload cost time and bytes: do not make them
        // for a CI that is not there.
        self.client.configured()?;

        let path = checkout_path(args.path.as_deref())?;
        let snapshot = tokio::task::spawn_blocking(move || snapshot::take(&path))
            .await
            .map_err(|e| format!("the snapshot failed: {e}"))??;
        let upload = snapshot.upload(&args.jobs);
        let reply = self
            .client
            .manual_upload(&upload, &snapshot.bundles)
            .await?;
        // The bundles are on the CI now.
        drop(snapshot);
        match reply {
            RunReply::Accepted { repo, number } => {
                let left = wait.saturating_sub(started.elapsed()).as_secs().max(1);
                self.ci_wait(CiWaitArgs {
                    repo,
                    number: Some(number),
                    sha: None,
                    job: None,
                    wait_seconds: Some(left),
                })
                .await
            }
            RunReply::Refused { message } => Err(message),
        }
    }

    /// Looks at a pipeline, named by its number or by the sha of its commit,
    /// until it has settled (or, with `job`, until that job has) or the wait
    /// has passed, and answers as `ci_pipeline` does. A sha that matches no
    /// pipeline yet is waited for: the push may not have reached the CI. Once
    /// it matches, the pipeline is followed by its number, so a newer one of
    /// the same commit does not take its place.
    async fn ci_wait(&self, args: CiWaitArgs) -> Result<String, String> {
        let CiWaitArgs {
            repo,
            number,
            sha,
            job,
            wait_seconds,
        } = args;
        let mut wanted = match (number, sha) {
            (Some(number), None) => Wanted::Number(number),
            (None, Some(sha)) if is_sha_prefix(&sha) => Wanted::Sha(sha),
            (None, Some(sha)) => return Err(format!("sha {sha:?} is not 4 to 40 hex digits")),
            _ => return Err("give either number or sha".to_owned()),
        };
        self.client.configured()?;
        let started = Instant::now();
        let deadline = started + wait_for(wait_seconds);
        let mut latest: Option<PipelineView> = None;
        // Every look sets it before the loop can end.
        let mut last_error: Option<String>;
        let mut failing_since: Option<Instant> = None;
        loop {
            let looked = match &wanted {
                Wanted::Number(number) => self.look(&repo, *number).await,
                Wanted::Sha(sha) => self.find(&repo, sha).await,
            };
            match looked {
                Ok(Some(view)) => {
                    wanted = Wanted::Number(view.number);
                    last_error = None;
                    failing_since = None;
                    let mut settled = view.finished.is_some();
                    if let Some(name) = &job {
                        let found =
                            view.jobs.iter().find(|j| j.name == *name).ok_or_else(|| {
                                let jobs: Vec<&str> =
                                    view.jobs.iter().map(|j| j.name.as_str()).collect();
                                format!(
                                    "no job {name} in {repo} #{}; its jobs are {}",
                                    view.number,
                                    jobs.join(", ")
                                )
                            })?;
                        settled |=
                            matches!(found.status.as_str(), "succeeded" | "failed" | "skipped");
                    }
                    if settled {
                        return Ok(pipeline_text(&view));
                    }
                    latest = Some(view);
                }
                Ok(None) => match &wanted {
                    Wanted::Number(number) => return Err(format!("no pipeline {repo} #{number}")),
                    Wanted::Sha(_) => {
                        last_error = None;
                        failing_since = None;
                    }
                },
                // A CI that restarts or a link that drops is not the end of
                // the wait: look again for a while.
                Err(e) => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= self.error_grace {
                        return Err(unreachable_text(&e, &wanted.describe(&repo), "ci_wait"));
                    }
                    last_error = Some(e);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(self.poll.min(deadline - now)).await;
        }
        let seconds = started.elapsed().as_secs();
        match (latest, &wanted, last_error) {
            (Some(view), _, _) => Ok(waiting_text(&view, job.as_deref(), seconds)),
            (None, wanted, Some(e)) => {
                Err(unreachable_text(&e, &wanted.describe(&repo), "ci_wait"))
            }
            (None, Wanted::Sha(sha), None) => Ok(format!(
                "no pipeline of {sha} in {repo} after {seconds} s; the push may not have \
                 reached the CI yet; call ci_wait again\n"
            )),
            (None, Wanted::Number(number), None) => Err(format!("no pipeline {repo} #{number}")),
        }
    }

    /// The newest pipeline of any channel whose commit id starts with `sha`;
    /// `None` while there is none.
    async fn find(&self, repo: &str, sha: &str) -> Result<Option<PipelineView>, String> {
        let request = CiRequest::Find {
            repo: repo.to_owned(),
            sha: sha.to_owned(),
        };
        match self.client.ci(request).await? {
            CiReply::Pipeline { pipeline } => Ok(pipeline.map(|view| *view)),
            other => Err(unexpected(&other)),
        }
    }

    /// The pipeline as the CI has it; `None` while it has no record.
    async fn look(&self, repo: &str, number: u64) -> Result<Option<PipelineView>, String> {
        let request = CiRequest::Pipeline {
            repo: repo.to_owned(),
            number,
        };
        match self.client.ci(request).await? {
            CiReply::Pipeline { pipeline } => Ok(pipeline.map(|view| *view)),
            other => Err(unexpected(&other)),
        }
    }

    async fn view(&self, repo: &str, number: u64) -> Result<PipelineView, String> {
        self.look(repo, number)
            .await?
            .ok_or_else(|| format!("no pipeline {repo} #{number}"))
    }
}

impl Tools for Toolbox {
    fn instructions(&self) -> &'static str {
        INSTRUCTIONS
    }

    fn definitions(&self) -> Vec<Value> {
        definitions()
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "ci_status" => {
                arguments_of::<NoArguments>(arguments)?;
                self.status().await
            }
            "ci_pipelines" => self.pipelines(arguments_of(arguments)?).await,
            "ci_pipeline" => self.pipeline(arguments_of(arguments)?).await,
            "ci_log" => self.log(arguments_of(arguments)?).await,
            "ci_timing" => self.timing(arguments_of(arguments)?).await,
            "ci_rerun" => self.rerun(arguments_of(arguments)?).await,
            "ci_cancel" => self.cancel(arguments_of(arguments)?).await,
            "ci_wait" => self.ci_wait(arguments_of(arguments)?).await,
            "run_jobs" => self.run_jobs(arguments_of(arguments)?).await,
            other => Err(format!(
                "unknown tool {other:?}; the tools are {}",
                definitions()
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

// The arguments of each tool. Strict: a name the tool does not know is a
// mistake the model should be told about, not one to ignore.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelinesArgs {
    repo: String,
    channel: String,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineArgs {
    repo: String,
    number: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogArgs {
    repo: String,
    number: u64,
    job: String,
    tail: Option<u32>,
    timestamps: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimingArgs {
    repo: String,
    number: u64,
    job: Option<String>,
    top: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunJobsArgs {
    jobs: Vec<String>,
    path: Option<String>,
    wait_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CiWaitArgs {
    repo: String,
    number: Option<u64>,
    sha: Option<String>,
    job: Option<String>,
    wait_seconds: Option<u64>,
}

/// What a `ci_wait` is waiting for: a pipeline by number, or the newest one
/// of a commit until it shows up.
enum Wanted {
    Number(u64),
    Sha(String),
}

impl Wanted {
    fn describe(&self, repo: &str) -> String {
        match self {
            Wanted::Number(number) => format!("{repo} #{number}"),
            Wanted::Sha(sha) => format!("the one of {sha} in {repo}"),
        }
    }
}

/// 4 to 40 hex digits: what the CI accepts as the start of a commit id.
fn is_sha_prefix(sha: &str) -> bool {
    (4..=40).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn arguments_of<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

fn wait_for(seconds: Option<u64>) -> Duration {
    Duration::from_secs(
        seconds
            .unwrap_or(DEFAULT_WAIT_SECONDS)
            .min(MAX_WAIT_SECONDS),
    )
}

/// The directory a manual run is taken from: the call's `path`, else the
/// process's working directory.
fn checkout_path(path: Option<&str>) -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    Ok(match path {
        // An absolute `path` replaces the working directory in a join.
        Some(path) if !path.is_empty() => cwd.join(path),
        _ => cwd,
    })
}

fn now() -> u64 {
    crate::enlist::now_secs()
}

fn unexpected(reply: &CiReply) -> String {
    format!("the CI answered something else than asked: {reply:?}")
}

/// Why a wait stopped: the CI could not be asked, and the pipeline it was
/// waiting for goes on without it. `again` is the tool that asks again.
fn unreachable_text(error: &str, pipeline: &str, again: &str) -> String {
    format!("{error} (the pipeline is {pipeline}; {again} asks again)")
}

fn accepted(reply: CiReply) -> Result<String, String> {
    match reply {
        CiReply::Accepted => Ok("accepted".to_owned()),
        other => Err(unexpected(&other)),
    }
}

// What the tools print.

/// `ci_status`: the CI at a glance.
fn status_text(overview: &CiOverview, now: u64) -> String {
    let mut text = String::new();
    section(
        &mut text,
        "channels",
        overview
            .channels
            .iter()
            .map(|c| format!("{}/{} #{} {}", c.repo, c.channel, c.latest, c.status)),
    );
    section(
        &mut text,
        "machines",
        overview.machines.iter().map(|m| {
            let mut line = format!("{} {} {}", m.name, m.kind, m.status);
            if !m.jobs.is_empty() {
                let jobs: Vec<String> = m
                    .jobs
                    .iter()
                    .map(|job| format!("{}#{}/{}", job.repo, job.number, job.job))
                    .collect();
                line.push_str(&format!(" [{}]", jobs.join(", ")));
            }
            line
        }),
    );
    section(
        &mut text,
        "queued",
        overview.queued.iter().map(|q| {
            format!(
                "{}/{} {} {} {} ({}) {} ago",
                q.repo,
                q.channel,
                q.event,
                q.git_ref,
                short(q.sha.as_deref()),
                q.actor,
                age(now.saturating_sub(q.received))
            )
        }),
    );
    section(
        &mut text,
        "platforms",
        overview
            .platforms
            .iter()
            .map(|(platform, target)| format!("{platform} -> {target}")),
    );
    text
}

fn section(text: &mut String, title: &str, lines: impl Iterator<Item = String>) {
    text.push_str(title);
    text.push('\n');
    let mut any = false;
    for line in lines {
        text.push_str("  ");
        text.push_str(&line);
        text.push('\n');
        any = true;
    }
    if !any {
        text.push_str("  none\n");
    }
}

/// `#<n> <status> <event> <ref> <sha7> <subject> (<actor>)`
fn pipeline_line(pipeline: &PipelineView) -> String {
    let mut line = format!(
        "#{} {} {} {} {}",
        pipeline.number,
        pipeline.status,
        pipeline.event,
        pipeline.git_ref,
        short(pipeline.sha.as_deref())
    );
    let subject = one_line(&pipeline.subject);
    if !subject.is_empty() {
        line.push(' ');
        line.push_str(&subject);
    }
    if !pipeline.actor.is_empty() {
        line.push_str(&format!(" ({})", pipeline.actor));
    }
    line
}

/// `<repo>/<channel>` and the pipeline's line: how every text about one
/// pipeline begins.
fn pipeline_header(pipeline: &PipelineView) -> String {
    format!(
        "{}/{} {}",
        pipeline.repo,
        pipeline.channel,
        pipeline_line(pipeline)
    )
}

/// `ci_pipeline`: the header, then every job; a failed job adds why.
fn pipeline_text(pipeline: &PipelineView) -> String {
    let mut text = pipeline_header(pipeline);
    text.push('\n');
    if !pipeline.note.is_empty() {
        text.push_str(&format!("note: {}\n", one_line(&pipeline.note)));
    }
    for job in &pipeline.jobs {
        text.push_str(&job_line(job));
        text.push('\n');
        if let Some(cause) = &job.cause {
            text.push_str(&format!("    cause: {}\n", one_line(cause)));
        }
        if let Some(excerpt) = &job.excerpt {
            text.push_str("    excerpt:\n");
            for line in excerpt.lines() {
                text.push_str("      ");
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    text
}

/// A `ci_wait` that stopped before what it waited for happened: the
/// pipeline as it stands, and how to wait on.
fn waiting_text(pipeline: &PipelineView, job: Option<&str>, waited_seconds: u64) -> String {
    let mut text = pipeline_text(pipeline);
    text.push_str(&format!(
        "still running after {waited_seconds} s; call ci_wait again with repo={} number={}",
        pipeline.repo, pipeline.number
    ));
    if let Some(job) = job {
        text.push_str(&format!(" job={job}"));
    }
    text.push('\n');
    text
}

/// `  <name> <status> <place> <note>`
fn job_line(job: &JobView) -> String {
    let note = one_line(&job.note);
    let parts = [job.name.as_str(), &job.status, &job.place, &note];
    format!(
        "  {}",
        parts
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// `ci_log`: the lines, and a note when the job may still add to them.
fn log_text(text: &str, finished: bool) -> String {
    let mut out = text.trim_end_matches('\n').to_owned();
    out.push('\n');
    if !finished {
        out.push_str("(the job still runs; more output may follow)\n");
    }
    out
}

/// `ci_timing` without a job: when the pipeline was created and how long it
/// took, then for every job when it was assigned, started and ended (seconds
/// after the pipeline's creation), how long it staged (assigned to started)
/// and ran, and under it the inputs that moved to its runner.
fn timing_text(pipeline: &PipelineView) -> String {
    let created = pipeline.created;
    let day = created % 86_400;
    let mut text = pipeline_header(pipeline);
    text.push_str(&format!(
        "\ncreated {:02}:{:02}:{:02} UTC, ",
        day / 3600,
        day % 3600 / 60,
        day % 60
    ));
    match pipeline.finished {
        Some(finished) => {
            text.push_str(&format!("took {} s\n", finished.saturating_sub(created)));
        }
        None => text.push_str("running\n"),
    }
    let offset = |at: Option<u64>| {
        at.map_or_else(
            || "-".to_owned(),
            |at| format!("+{} s", at.saturating_sub(created)),
        )
    };
    let between = |from: Option<u64>, to: Option<u64>| match (from, to) {
        (Some(from), Some(to)) => format!("{} s", to.saturating_sub(from)),
        _ => "-".to_owned(),
    };
    for job in &pipeline.jobs {
        text.push_str(&format!(
            "  {:<20} {:<9} assigned {} started {} ended {} staging {} ran {}\n",
            job.name,
            job.status,
            offset(job.assigned),
            offset(job.started),
            offset(job.finished),
            between(job.assigned, job.started),
            between(job.started, job.finished),
        ));
        for transfer in &job.transfers {
            text.push_str(&transfer_line(transfer, job.assigned));
            text.push('\n');
        }
    }
    text
}

/// `    input <need>: <size> MB in <s> s`, with `(fetched early)` when the
/// set was there before the job was assigned; `failed: <error>` or
/// `receiving` in place of both when it did not arrive or has not yet.
fn transfer_line(transfer: &TransferView, assigned: Option<u64>) -> String {
    let what = match (&transfer.error, transfer.finished) {
        (Some(error), _) => format!("failed: {}", one_line(error)),
        (None, None) => "receiving".to_owned(),
        (None, Some(finished)) => {
            let size = transfer.bytes.map_or_else(
                || "-".to_owned(),
                |bytes| format!("{:.1} MB", bytes as f64 / 1e6),
            );
            let early = if assigned.is_some_and(|assigned| finished <= assigned) {
                " (fetched early)"
            } else {
                ""
            };
            format!(
                "{size} in {} s{early}",
                finished.saturating_sub(transfer.started)
            )
        }
    };
    format!("    input {}: {what}", transfer.need)
}

/// `ci_timing` with a job: how much output it has and how long it spans,
/// then the lines that the longest pauses follow, each with when it began
/// (seconds after the job's first output) and how long it stood.
fn job_timing_text(repo: &str, number: u64, job: &str, timing: &JobTiming) -> String {
    let seconds = |ms: u64| ms as f64 / 1000.0;
    let mut text = format!(
        "{repo} #{number} {job}: {} lines over {:.1} s of output",
        timing.lines,
        seconds(timing.span_ms)
    );
    if !timing.finished {
        text.push_str(", still running");
    }
    text.push('\n');
    if timing.truncated {
        text.push_str(&format!(
            "(only the last {} MiB of output were read)\n",
            TIMING_READ_BYTES >> 20
        ));
    }
    if !timing.slowest.is_empty() {
        text.push_str(&format!(
            "the {} lines followed by the longest pauses, in output order:\n",
            timing.slowest.len()
        ));
        for line in &timing.slowest {
            text.push_str(&format!(
                "  +{:.1} s  {:.1} s  {}\n",
                seconds(line.at_ms),
                seconds(line.took_ms),
                line.text
            ));
        }
    }
    text
}

/// The first seven characters of a commit id, `-` when there is none.
fn short(sha: Option<&str>) -> &str {
    match sha {
        Some(sha) if !sha.is_empty() => sha.get(..7).unwrap_or(sha),
        _ => "-",
    }
}

/// Runs of whitespace, newlines included, as single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn age(seconds: u64) -> String {
    match seconds {
        0..120 => format!("{seconds} s"),
        120..7200 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
}

// What the agent is told about the tools.

fn definitions() -> Vec<Value> {
    let repo = json!({
        "type": "string",
        "description": "Repository name as ci_status lists it, for example zeughaus.",
    });
    let number = json!({
        "type": "integer",
        "minimum": 1,
        "description": "Pipeline number.",
    });
    let wait = |what: &str| {
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": MAX_WAIT_SECONDS,
            "default": DEFAULT_WAIT_SECONDS,
            "description": what,
        })
    };
    vec![
        tool(
            "ci_status",
            "The CI at a glance: every channel with its newest pipeline and status, every \
             runner and machine with its state and the jobs on it, the events waiting in the \
             queue, and the platforms (name -> runner or machine). Start here.",
            json!({}),
            &[],
            true,
        ),
        tool(
            "ci_pipelines",
            "A channel's pipelines, newest first, one line each: #number status event ref \
             sha subject (actor).",
            json!({
                "repo": repo,
                "channel": {
                    "type": "string",
                    "description": "Channel name as ci_status lists it, for example dev, \
                                    nightly or release.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_PIPELINES_PER_REQUEST,
                    "default": DEFAULT_PIPELINES,
                    "description": "How many pipelines to list.",
                },
            }),
            &["repo", "channel"],
            true,
        ),
        tool(
            "ci_pipeline",
            "One pipeline: its header line, then every job with status, place and note. A \
             failed job adds the line that says why it failed and the stored excerpt of its \
             output.",
            json!({ "repo": repo, "number": number }),
            &["repo", "number"],
            true,
        ),
        tool(
            "ci_log",
            "The last lines of one job's output in a pipeline, live while the job runs.",
            json!({
                "repo": repo,
                "number": number,
                "job": {
                    "type": "string",
                    "description": "Job name as ci_pipeline lists it.",
                },
                "tail": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TAIL,
                    "default": DEFAULT_TAIL,
                    "description": "How many lines from the end.",
                },
                "timestamps": {
                    "type": "boolean",
                    "default": false,
                    "description": "Prefix every line with the seconds since the job's first \
                                    output.",
                },
            }),
            &["repo", "number", "job"],
            true,
        ),
        tool(
            "ci_timing",
            "Where a pipeline's time went. Without job: each job's assignment, start and end \
             relative to the pipeline's creation, its staging time and the inputs it received. \
             With job: the lines of that job's output that the longest pauses follow, with \
             when each began and how long it stood.",
            json!({
                "repo": repo,
                "number": number,
                "job": {
                    "type": "string",
                    "description": "Job name as ci_pipeline lists it: tell where the time of \
                                    its output went instead of the pipeline's.",
                },
                "top": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMING_LINES,
                    "default": DEFAULT_TOP,
                    "description": "With job: how many lines to name.",
                },
            }),
            &["repo", "number"],
            true,
        ),
        tool(
            "ci_wait",
            "Wait until a pipeline, or one job of it, has settled, and answer with the pipeline \
             as ci_pipeline does. Name the pipeline by number, or by the sha of its commit right \
             after a push: a pipeline that does not exist yet is waited for.",
            json!({
                "repo": repo,
                "number": number,
                "sha": {
                    "type": "string",
                    "pattern": "^[0-9a-fA-F]{4,40}$",
                    "description": "The start of the commit id, instead of number: the newest \
                                    pipeline of that commit in any channel.",
                },
                "job": {
                    "type": "string",
                    "description": "Wait for this job only; it has settled when it ended or \
                                    was skipped because a job it needs failed.",
                },
                "wait_seconds": wait("How long to wait for it to settle."),
            }),
            &["repo"],
            true,
        ),
        tool(
            "ci_rerun",
            "Queue the pipeline's event again, pinned to the commit it ran for. Answers \
             `accepted`. A manual run cannot be rerun: start a new one with run_jobs.",
            json!({ "repo": repo, "number": number }),
            &["repo", "number"],
            false,
        ),
        tool(
            "ci_cancel",
            "Stop a running pipeline: its jobs are killed and the rest are skipped. Answers \
             `accepted`.",
            json!({ "repo": repo, "number": number }),
            &["repo", "number"],
            false,
        ),
        tool(
            "run_jobs",
            "Run jobs of this repository's pipeline from the current working tree, each in the \
             warm workspace of its place, where the pipeline builds too, so builds and tests \
             are incremental. The checkout at `path` is snapshotted as it is on disk (its \
             commit, the changes to it, untracked files that are not ignored) with the sibling \
             checkouts its Cargo path dependencies point to. Only the named jobs run; a need \
             on a job that is not named is dropped, and no secrets are given. Answers as \
             ci_wait does once the run has settled, or says how to continue with ci_wait.",
            json!({
                "jobs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": MAX_MANUAL_JOBS,
                    "description": "Job names: the files of .zeughaus-ci/ without extension, \
                                    e.g. build-windows-amd64.",
                },
                "path": {
                    "type": "string",
                    "description": "A directory inside the checkout to snapshot. Default: \
                                    the working directory of this server.",
                },
                "wait_seconds": wait(
                    "How long this call may take in all, snapshot and upload included. A \
                     run that has not settled by then is picked up with ci_wait."
                ),
            }),
            &["jobs"],
            false,
        ),
    ]
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": { "readOnlyHint": read_only },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::io::AsyncReadExt;
    use weida::TransferMeta;
    use zeughaus_link::enlist::UploadDir;
    use zeughaus_link::enlist::{read_frame, write_frame};
    use zeughaus_link::manual::{RunReply, RunUpload};
    use zeughaus_link::{
        CI_PATH, ChannelView, MANUAL_PATH, MachineJob, MachineView, QueuedView, TimedLine,
    };

    use super::*;
    use crate::transport::Transport;

    fn job(name: &str, status: &str, place: &str, note: &str) -> JobView {
        JobView {
            name: name.to_owned(),
            needs: Vec::new(),
            place: place.to_owned(),
            status: status.to_owned(),
            note: note.to_owned(),
            started: Some(1_000),
            finished: Some(1_012),
            code: None,
            cause: None,
            excerpt: None,
            assigned: None,
            transfers: Vec::new(),
        }
    }

    fn pipeline(status: &str, finished: Option<u64>, jobs: Vec<JobView>) -> PipelineView {
        PipelineView {
            repo: "demo".to_owned(),
            number: 7,
            channel: "dev".to_owned(),
            event: "manual".to_owned(),
            git_ref: "main".to_owned(),
            sha: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            actor: "hannes@tuco-pc".to_owned(),
            subject: "cargo test".to_owned(),
            status: status.to_owned(),
            note: String::new(),
            created: 1_000,
            finished,
            jobs,
        }
    }

    #[test]
    fn there_are_nine_tools_and_each_takes_an_object() {
        let definitions = definitions();
        let names: Vec<&str> = definitions
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "ci_status",
                "ci_pipelines",
                "ci_pipeline",
                "ci_log",
                "ci_timing",
                "ci_wait",
                "ci_rerun",
                "ci_cancel",
                "run_jobs"
            ]
        );
        for tool in &definitions {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(tool["description"].as_str().unwrap().len() > 20, "{tool}");
            // A property that is required must exist.
            for required in tool["inputSchema"]["required"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let key = required.as_str().unwrap();
                assert!(tool["inputSchema"]["properties"][key].is_object(), "{tool}");
            }
        }
        fn schema<'a>(definitions: &'a [Value], name: &str) -> &'a Value {
            &definitions
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("no tool {name}"))["inputSchema"]
        }
        let run = schema(&definitions, "run_jobs");
        assert_eq!(run["required"], json!(["jobs"]));
        assert_eq!(run["properties"]["jobs"]["minItems"], 1);
        assert_eq!(run["properties"]["jobs"]["maxItems"], 16);
        assert_eq!(run["properties"]["wait_seconds"]["default"], 540);
        assert_eq!(run["properties"]["wait_seconds"]["maximum"], 600);
        assert_eq!(definitions[8]["annotations"]["readOnlyHint"], false);
        let log = schema(&definitions, "ci_log");
        assert_eq!(log["required"], json!(["repo", "number", "job"]));
        assert_eq!(log["properties"]["timestamps"]["type"], "boolean");
        assert_eq!(log["properties"]["timestamps"]["default"], false);
        let timing = schema(&definitions, "ci_timing");
        assert_eq!(timing["required"], json!(["repo", "number"]));
        assert_eq!(timing["properties"]["top"]["default"], 15);
        assert_eq!(timing["properties"]["top"]["maximum"], 100);
    }

    #[test]
    fn the_status_names_channels_machines_the_queue_and_the_platforms() {
        let overview = CiOverview {
            channels: vec![
                ChannelView {
                    repo: "zeughaus".to_owned(),
                    channel: "dev".to_owned(),
                    latest: 123,
                    status: "succeeded".to_owned(),
                },
                ChannelView {
                    repo: "zeughaus".to_owned(),
                    channel: "nightly".to_owned(),
                    latest: 9,
                    status: "running".to_owned(),
                },
            ],
            machines: vec![
                MachineView {
                    name: "tuco-pc".to_owned(),
                    kind: "runner".to_owned(),
                    status: "free".to_owned(),
                    jobs: vec![MachineJob {
                        repo: "zeughaus".to_owned(),
                        channel: "nightly".to_owned(),
                        number: 9,
                        job: "build".to_owned(),
                    }],
                },
                MachineView {
                    name: "win11".to_owned(),
                    kind: "windows-vm".to_owned(),
                    status: "off".to_owned(),
                    jobs: Vec::new(),
                },
            ],
            queued: vec![QueuedView {
                repo: "zeughaus".to_owned(),
                channel: "dev".to_owned(),
                event: "push".to_owned(),
                git_ref: "main".to_owned(),
                sha: Some("abcdef0123".to_owned()),
                actor: "rerun of #6".to_owned(),
                received: 900,
            }],
            platforms: vec![
                ("linux-amd64".to_owned(), "tuco-pc".to_owned()),
                ("windows-amd64".to_owned(), "wohnzimmer".to_owned()),
            ],
        };
        assert_eq!(
            status_text(&overview, 1_000),
            "channels\n  zeughaus/dev #123 succeeded\n  zeughaus/nightly #9 running\n\
             machines\n  tuco-pc runner free [zeughaus#9/build]\n  win11 windows-vm off\n\
             queued\n  zeughaus/dev push main abcdef0 (rerun of #6) 100 s ago\n\
             platforms\n  linux-amd64 -> tuco-pc\n  windows-amd64 -> wohnzimmer\n"
        );
        // Nothing to list is said, not left out.
        let empty = CiOverview {
            channels: Vec::new(),
            machines: Vec::new(),
            queued: Vec::new(),
            platforms: Vec::new(),
        };
        assert_eq!(
            status_text(&empty, 0),
            "channels\n  none\nmachines\n  none\nqueued\n  none\nplatforms\n  none\n"
        );
    }

    #[test]
    fn ages_are_short() {
        assert_eq!(age(5), "5 s");
        assert_eq!(age(119), "119 s");
        assert_eq!(age(120), "2 min");
        assert_eq!(age(7199), "119 min");
        assert_eq!(age(7200), "2 h");
    }

    #[test]
    fn a_pipeline_is_one_line_and_a_failed_job_says_why() {
        let mut failed = job("test", "failed", "tuco-pc", "");
        failed.cause = Some("error[E0425]: cannot find value `x`".to_owned());
        failed.excerpt = Some("line one\nline two\n".to_owned());
        let mut view = pipeline(
            "failed",
            Some(1_012),
            vec![job("build", "succeeded", "tuco-pc", ""), failed],
        );
        view.channel = "dev".to_owned();
        view.event = "push".to_owned();
        view.git_ref = "main".to_owned();
        view.subject = "fix(ci): keep\nthe header short".to_owned();
        view.note = "one job failed".to_owned();
        assert_eq!(
            pipeline_line(&view),
            "#7 failed push main 0123456 fix(ci): keep the header short (hannes@tuco-pc)"
        );
        assert_eq!(
            pipeline_text(&view),
            "demo/dev #7 failed push main 0123456 fix(ci): keep the header short \
             (hannes@tuco-pc)\n\
             note: one job failed\n\
             \x20 build succeeded tuco-pc\n\
             \x20 test failed tuco-pc\n\
             \x20   cause: error[E0425]: cannot find value `x`\n\
             \x20   excerpt:\n\
             \x20     line one\n\
             \x20     line two\n"
        );
        // A record from an older CI has no subject, actor or commit.
        view.subject.clear();
        view.actor.clear();
        view.sha = None;
        assert_eq!(pipeline_line(&view), "#7 failed push main -");
    }

    #[test]
    fn a_log_says_when_the_job_still_runs() {
        assert_eq!(log_text("a\nb\n\n", true), "a\nb\n");
        assert_eq!(
            log_text("a", false),
            "a\n(the job still runs; more output may follow)\n"
        );
    }

    #[test]
    fn a_pipelines_time_is_told_per_job_with_what_moved_into_it() {
        let timed = |name: &str, status: &str, times: [Option<u64>; 3]| {
            let mut job = job(name, status, "tuco-pc", "");
            [job.assigned, job.started, job.finished] = times;
            job
        };
        let transfer = |need: &str, started, finished, bytes, error: Option<&str>| TransferView {
            need: need.to_owned(),
            started,
            finished,
            bytes,
            error: error.map(str::to_owned),
        };
        let a = timed("a", "succeeded", [Some(45_000), Some(45_002), Some(45_010)]);
        let b = timed("b", "succeeded", [Some(45_000), Some(45_001), Some(45_030)]);
        // `a` reached the runner of `c` while `b` still ran, before `c` was
        // assigned.
        let mut c = timed("c", "succeeded", [Some(45_030), Some(45_031), Some(45_040)]);
        c.transfers = vec![transfer("a", 45_010, Some(45_016), Some(300_000_000), None)];
        // `d` fetched what `c` made after it was assigned, or in the very
        // second it was; one input broke and another is still on its way.
        let mut d = timed("d", "failed", [Some(45_040), Some(45_046), Some(45_058)]);
        d.transfers = vec![
            transfer("c", 45_041, Some(45_045), Some(1_500_000), None),
            transfer(
                "x",
                45_041,
                Some(45_042),
                None,
                Some("the upload ended\nbefore the end of the set"),
            ),
            transfer("y", 45_042, None, None, None),
            transfer("z", 45_039, Some(45_040), Some(0), None),
        ];
        let e = timed("e", "skipped", [None; 3]);
        let mut view = pipeline("failed", Some(45_060), vec![a, b, c, d, e]);
        view.created = 45_000;
        view.channel = "dev".to_owned();
        view.event = "push".to_owned();
        view.git_ref = "main".to_owned();
        assert_eq!(
            timing_text(&view),
            concat!(
                "demo/dev #7 failed push main 0123456 cargo test (hannes@tuco-pc)\n",
                "created 12:30:00 UTC, took 60 s\n",
                "  a                    succeeded assigned +0 s started +2 s ended +10 s staging 2 s ran 8 s\n",
                "  b                    succeeded assigned +0 s started +1 s ended +30 s staging 1 s ran 29 s\n",
                "  c                    succeeded assigned +30 s started +31 s ended +40 s staging 1 s ran 9 s\n",
                "    input a: 300.0 MB in 6 s (fetched early)\n",
                "  d                    failed    assigned +40 s started +46 s ended +58 s staging 6 s ran 12 s\n",
                "    input c: 1.5 MB in 4 s\n",
                "    input x: failed: the upload ended before the end of the set\n",
                "    input y: receiving\n",
                "    input z: 0.0 MB in 1 s (fetched early)\n",
                "  e                    skipped   assigned - started - ended - staging - ran -\n",
            )
        );

        // A pipeline that has not finished has no duration yet.
        let mut going = pipeline(
            "running",
            None,
            vec![
                timed("a", "running", [Some(45_000), Some(45_002), None]),
                timed("b", "pending", [None; 3]),
            ],
        );
        going.created = 45_000;
        assert_eq!(
            timing_text(&going),
            concat!(
                "demo/dev #7 running manual main 0123456 cargo test (hannes@tuco-pc)\n",
                "created 12:30:00 UTC, running\n",
                "  a                    running   assigned +0 s started +2 s ended - staging 2 s ran -\n",
                "  b                    pending   assigned - started - ended - staging - ran -\n",
            )
        );
    }

    #[test]
    fn the_lines_a_jobs_longest_pauses_follow_come_with_when_and_how_long() {
        let line = |at_ms, took_ms, text: &str| TimedLine {
            at_ms,
            took_ms,
            text: text.to_owned(),
        };
        let mut timing = JobTiming {
            lines: 4,
            span_ms: 40_000,
            slowest: vec![
                line(200, 30_000, "compiling a"),
                line(31_000, 9_040, "link"),
            ],
            finished: false,
            truncated: false,
        };
        assert_eq!(
            job_timing_text("demo", 7, "build", &timing),
            "demo #7 build: 4 lines over 40.0 s of output, still running\n\
             the 2 lines followed by the longest pauses, in output order:\n\
             \x20 +0.2 s  30.0 s  compiling a\n\
             \x20 +31.0 s  9.0 s  link\n"
        );

        // A job that ended, of whose output only the end was read.
        timing.finished = true;
        timing.truncated = true;
        let text = job_timing_text("demo", 7, "build", &timing);
        assert!(
            text.starts_with(
                "demo #7 build: 4 lines over 40.0 s of output\n\
                 (only the last 64 MiB of output were read)\n\
                 the 2 lines followed"
            ),
            "{text}"
        );

        // Output that has not begun has no lines to name.
        let nothing = JobTiming {
            lines: 0,
            span_ms: 0,
            slowest: Vec::new(),
            finished: true,
            truncated: false,
        };
        assert_eq!(
            job_timing_text("demo", 7, "build", &nothing),
            "demo #7 build: 0 lines over 0.0 s of output\n"
        );
    }

    fn toolbox_without_a_ci() -> Toolbox {
        Toolbox::new(Arc::new(Client::new(Err(
            "no CI configured: add ci = [..] to /x/zeughaus.toml".to_owned(),
        ))))
    }

    #[tokio::test]
    async fn without_a_ci_every_tool_says_why() {
        let tools = toolbox_without_a_ci();
        for (name, arguments) in [
            ("ci_status", json!({})),
            ("ci_pipelines", json!({ "repo": "r", "channel": "c" })),
            ("ci_pipeline", json!({ "repo": "r", "number": 1 })),
            ("ci_log", json!({ "repo": "r", "number": 1, "job": "j" })),
            (
                "ci_log",
                json!({ "repo": "r", "number": 1, "job": "j", "timestamps": true }),
            ),
            ("ci_timing", json!({ "repo": "r", "number": 1 })),
            (
                "ci_timing",
                json!({ "repo": "r", "number": 1, "job": "j", "top": 3 }),
            ),
            ("ci_rerun", json!({ "repo": "r", "number": 1 })),
            ("ci_cancel", json!({ "repo": "r", "number": 1 })),
            ("ci_wait", json!({ "repo": "r", "number": 1 })),
            ("run_jobs", json!({ "jobs": ["build-x"] })),
        ] {
            let err = tools.call(name, arguments).await.expect_err(name);
            assert!(err.starts_with("no CI configured"), "{name}: {err}");
        }
    }

    #[tokio::test]
    async fn arguments_are_checked_before_anything_is_done() {
        let tools = toolbox_without_a_ci();
        let unknown = tools.call("ci_nothing", json!({})).await.err().unwrap();
        assert!(unknown.contains("unknown tool \"ci_nothing\""), "{unknown}");
        assert!(unknown.contains("run_jobs"), "{unknown}");

        let many: Vec<String> = (0..=MAX_MANUAL_JOBS).map(|i| format!("job-{i}")).collect();
        for (arguments, expected) in [
            (json!({}), "missing field `jobs`"),
            (json!({ "jobs": ["a"], "wait": 1 }), "unknown field `wait`"),
            (json!({ "jobs": "a" }), "invalid type"),
            (
                json!({ "jobs": ["a"], "wait_seconds": "5" }),
                "invalid type",
            ),
            (
                json!({ "jobs": [] }),
                "jobs has 0 entries; it must have 1 to 16",
            ),
            (
                json!({ "jobs": many }),
                "jobs has 17 entries; it must have 1 to 16",
            ),
            (json!({ "jobs": ["Build Windows"] }), "is not a job name"),
            (json!({ "jobs": ["a/b"] }), "is not a job name"),
            (json!({ "jobs": ["a", "b", "a"] }), "job a is named twice"),
        ] {
            let err = tools
                .call("run_jobs", arguments.clone())
                .await
                .err()
                .unwrap();
            assert!(err.contains(expected), "{arguments}: {err}");
        }
        let err = tools
            .call("ci_status", json!({ "repo": "x" }))
            .await
            .err()
            .unwrap();
        assert!(err.contains("unknown field `repo`"), "{err}");
    }

    #[test]
    fn a_wait_is_bounded_by_what_the_clients_call_allows() {
        assert_eq!(wait_for(None), Duration::from_secs(540));
        assert_eq!(wait_for(Some(30)), Duration::from_secs(30));
        assert_eq!(wait_for(Some(0)), Duration::ZERO);
        assert_eq!(wait_for(Some(86_400)), Duration::from_secs(600));
    }

    /// What the fake CI below was asked.
    #[derive(Default)]
    struct Seen {
        uploads: Mutex<Vec<(RunUpload, Vec<Vec<u8>>)>>,
        asked: Mutex<Vec<CiRequest>>,
        /// Looks at the pipeline before it is finished.
        running_looks: AtomicU32,
        /// Asks to find a pipeline by sha.
        finds: AtomicU32,
    }

    /// A weida listener that answers `/ci` and `/manual` the way the CI does,
    /// with a pipeline that runs for `running_looks` looks, then ends as
    /// `outcome`.
    struct FakeCi {
        _transport: Transport,
        state: PathBuf,
        url: String,
        seen: Arc<Seen>,
    }

    impl Drop for FakeCi {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.state);
        }
    }

    async fn fake_ci(tag: &str, running_looks: u32, refuse: Option<&str>) -> FakeCi {
        let state =
            std::env::temp_dir().join(format!("zeughaus-mcp-fake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let transport = Transport::start(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &state)
            .await
            .expect("fake CI");
        let seen = Arc::new(Seen::default());
        let ci = transport.listener().replier(CI_PATH).expect("/ci");
        tokio::spawn(serve_ci(ci, Arc::clone(&seen), running_looks));
        let manual = transport.listener().replier(MANUAL_PATH).expect("/manual");
        tokio::spawn(serve_manual(
            manual,
            Arc::clone(&seen),
            refuse.map(str::to_owned),
        ));
        FakeCi {
            url: transport.url().to_owned(),
            _transport: transport,
            state,
            seen,
        }
    }

    /// Pipeline 1 of the fake: running for `running_looks` looks, then done.
    fn pipeline_one(seen: &Seen, running_looks: u32) -> PipelineView {
        let looks = seen.running_looks.fetch_add(1, Ordering::Relaxed);
        let mut view = if looks < running_looks {
            pipeline("running", None, vec![job("build", "running", "agent", "")])
        } else {
            let mut done = job("build", "succeeded", "agent", "");
            done.code = Some(0);
            pipeline("succeeded", Some(1_007), vec![done])
        };
        view.number = 1;
        view
    }

    async fn serve_ci(replier: weida::Replier, seen: Arc<Seen>, running_looks: u32) {
        while let Ok(mut request) = replier.accept().await {
            let body = request
                .take_body()
                .collect(zeughaus_link::MAX_CI_REQUEST_BYTES)
                .await
                .expect("request");
            let asked = CiRequest::decode(&body).expect("a CiRequest");
            seen.asked.lock().unwrap().push(asked.clone());
            let reply = match asked {
                CiRequest::Pipeline { number: 1, .. } => CiReply::Pipeline {
                    pipeline: Some(Box::new(pipeline_one(&seen, running_looks))),
                },
                // Still running, one of its jobs already settled.
                CiRequest::Pipeline { number: 4, .. } => {
                    let mut view = pipeline(
                        "running",
                        None,
                        vec![
                            job("build", "running", "agent", ""),
                            job("publish", "skipped", "agent", ""),
                        ],
                    );
                    view.number = 4;
                    CiReply::Pipeline {
                        pipeline: Some(Box::new(view)),
                    }
                }
                // The push of `dead..` reaches the CI after two asks.
                CiRequest::Find { sha, .. } if sha.starts_with("dead") => {
                    if seen.finds.fetch_add(1, Ordering::Relaxed) < 2 {
                        CiReply::Pipeline { pipeline: None }
                    } else {
                        CiReply::Pipeline {
                            pipeline: Some(Box::new(pipeline_one(&seen, running_looks))),
                        }
                    }
                }
                CiRequest::Find { .. } => CiReply::Pipeline { pipeline: None },
                // What a CI says while it cannot read its records.
                CiRequest::Pipeline { number: 3, .. } => CiReply::Error {
                    message: "the scheduler is restarting".to_owned(),
                },
                CiRequest::Pipeline { .. } => CiReply::Pipeline { pipeline: None },
                CiRequest::Log {
                    job,
                    tail,
                    timestamps,
                    ..
                } => CiReply::Log {
                    text: format!(
                        "{}job {job}, {tail} lines\ntest result: ok. 1 passed\n",
                        if timestamps { "[+     0.0s] " } else { "" }
                    ),
                    finished: true,
                },
                CiRequest::Timing { job, .. } if job == "nothing" => CiReply::Error {
                    message: "no output for nothing of #1".to_owned(),
                },
                CiRequest::Timing { job, top, .. } => CiReply::Timing {
                    timing: JobTiming {
                        lines: 3,
                        span_ms: 31_200,
                        slowest: vec![TimedLine {
                            at_ms: 0,
                            took_ms: 30_000,
                            text: format!("{job}: asked for {top}"),
                        }],
                        finished: false,
                        truncated: true,
                    },
                },
                _ => CiReply::Error {
                    message: "not served by the fake".to_owned(),
                },
            };
            let mut out = request.reply(TransferMeta::default()).await.expect("reply");
            out.write_all(&reply.encode()).await.expect("write");
            out.finish().expect("finish");
        }
    }

    async fn serve_manual(replier: weida::Replier, seen: Arc<Seen>, refuse: Option<String>) {
        while let Ok(mut request) = replier.accept().await {
            let mut body = request.take_body();
            let upload: RunUpload = read_frame(&mut body)
                .await
                .expect("frame")
                .expect("an upload");
            let reply = match &refuse {
                Some(message) => RunReply::Refused {
                    message: message.clone(),
                },
                None => {
                    let mut bundles = Vec::new();
                    for size in &upload.sizes {
                        let mut bundle = vec![0u8; *size as usize];
                        body.read_exact(&mut bundle).await.expect("a whole bundle");
                        bundles.push(bundle);
                    }
                    // Nothing follows the bundles.
                    let mut rest = Vec::new();
                    body.read_to_end(&mut rest).await.expect("the end");
                    assert!(rest.is_empty());
                    seen.uploads.lock().unwrap().push((upload, bundles));
                    RunReply::Accepted {
                        repo: "demo".to_owned(),
                        number: 1,
                    }
                }
            };
            let mut out = request.reply(TransferMeta::default()).await.expect("reply");
            write_frame(&mut out, &reply).await.expect("frame");
            out.finish().expect("finish");
        }
    }

    /// A tools box that talks to `ci` with the client key its transport made.
    fn toolbox_for(ci: &FakeCi) -> Toolbox {
        let link = super::super::client::Link::resolve(Some(ci.url.clone()), &ci.state);
        Toolbox::new(Arc::new(Client::new(link))).polling_every(Duration::from_millis(20))
    }

    fn checkout(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zeughaus-mcp-demo-{tag}-{}/demo",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/file.txt"), "in a subdirectory\n").unwrap();
        std::fs::write(dir.join("note.txt"), "untracked\n").unwrap();
        for args in [
            &["init", "-q"][..],
            &["remote", "add", "origin", "git@example.invalid:o/demo.git"][..],
        ] {
            let status = std::process::Command::new("git")
                .current_dir(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        dir
    }

    /// The whole of a manual run against a CI that is not the real one: the
    /// snapshot is taken from a directory, uploaded, and the pipeline the CI
    /// numbered is waited for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_manual_run_uploads_the_snapshot_and_waits_for_the_pipeline() {
        let ci = fake_ci("run", 2, None).await;
        let tools = toolbox_for(&ci);
        let dir = checkout("run");

        let text = tools
            .call(
                "run_jobs",
                json!({
                    "jobs": ["build-x", "check-x"],
                    "path": dir.join("sub"),
                    "wait_seconds": 30,
                }),
            )
            .await
            .expect("the run");

        assert!(
            text.starts_with("demo/dev #1 succeeded manual main 0123456"),
            "{text}"
        );
        assert!(text.contains("\n  build succeeded agent\n"), "{text}");
        assert!(!text.contains("still running"), "{text}");

        let uploads = ci.seen.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        let (upload, bundles) = &uploads[0];
        assert_eq!(upload.origin, "git@example.invalid:o/demo.git");
        assert_eq!(upload.jobs, ["build-x", "check-x"]);
        assert_eq!(upload.summary, "working tree");
        assert_eq!(upload.dirs.len(), 1);
        assert_eq!(upload.dirs[0].dir, "demo");
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].len() as u64, upload.sizes[0]);
        assert!(
            bundles[0].starts_with(b"# v2 git bundle\n")
                || bundles[0].starts_with(b"# v3 git bundle\n"),
            "{:?}",
            String::from_utf8_lossy(&bundles[0][..20])
        );
        // The pipeline was looked at while it ran and once when it was done,
        // and nothing else was asked.
        assert_eq!(ci.seen.running_looks.load(Ordering::Relaxed), 3);
        assert!(
            ci.seen
                .asked
                .lock()
                .unwrap()
                .iter()
                .all(|asked| matches!(asked, CiRequest::Pipeline { number: 1, .. }))
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_manual_run_that_outlasts_the_wait_says_how_to_continue() {
        let ci = fake_ci("again", 1_000, None).await;
        let tools = toolbox_for(&ci);
        let dir = checkout("again");

        let text = tools
            .call(
                "run_jobs",
                json!({ "jobs": ["build-x"], "path": dir, "wait_seconds": 1 }),
            )
            .await
            .expect("still running is not an error");
        assert!(
            text.starts_with("demo/dev #1 running manual main"),
            "{text}"
        );
        assert!(
            text.ends_with("still running after 1 s; call ci_wait again with repo=demo number=1\n"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A CI that answers with an error is looked at again for a while, in
    /// case it is a restart, and is then given up on, with the way back to
    /// the run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_ci_that_keeps_failing_ends_the_wait_with_its_error() {
        let ci = fake_ci("failing", 0, None).await;
        let tools = toolbox_for(&ci).tolerating_errors_for(Duration::from_millis(100));
        let started = Instant::now();
        let err = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "number": 3, "wait_seconds": 30 }),
            )
            .await
            .expect_err("the CI fails");
        assert_eq!(
            err,
            "the scheduler is restarting (the pipeline is demo #3; ci_wait asks again)"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it was told it could wait 30 s, not that it had to"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_wait_returns_once_the_pipeline_has_finished() {
        let ci = fake_ci("wait", 2, None).await;
        let tools = toolbox_for(&ci);
        let text = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "number": 1, "wait_seconds": 30 }),
            )
            .await
            .expect("the pipeline finishes");
        assert!(
            text.starts_with("demo/dev #1 succeeded manual main 0123456"),
            "{text}"
        );
        assert!(!text.contains("still running"), "{text}");
        assert_eq!(ci.seen.running_looks.load(Ordering::Relaxed), 3);

        // A number the CI does not know is an error at once, not a wait.
        let err = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "number": 2, "wait_seconds": 30 }),
            )
            .await
            .expect_err("unknown pipeline");
        assert_eq!(err, "no pipeline demo #2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_wait_for_a_job_returns_when_that_job_has_settled() {
        let ci = fake_ci("job", 0, None).await;
        let tools = toolbox_for(&ci);
        // The pipeline runs on, the skipped job is settled.
        let text = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "number": 4, "job": "publish", "wait_seconds": 30 }),
            )
            .await
            .expect("publish is skipped");
        assert!(text.contains("\n  publish skipped agent\n"), "{text}");
        assert!(text.contains("\n  build running agent\n"), "{text}");
        assert!(!text.contains("still running"), "{text}");

        // A job that has not settled by the end of the wait says how to go on.
        let text = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "number": 4, "job": "build", "wait_seconds": 0 }),
            )
            .await
            .expect("still running is not an error");
        assert!(
            text.ends_with(
                "still running after 0 s; call ci_wait again with repo=demo number=4 job=build\n"
            ),
            "{text}"
        );

        let err = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "number": 4, "job": "deploy" }),
            )
            .await
            .expect_err("no such job");
        assert_eq!(err, "no job deploy in demo #4; its jobs are build, publish");
    }

    /// A pipeline named by the sha of its commit may not exist yet: it is
    /// asked for until it does, and then followed by its number.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_wait_for_a_sha_waits_for_the_pipeline_to_appear() {
        let ci = fake_ci("sha", 1, None).await;
        let tools = toolbox_for(&ci);
        let text = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "sha": "deadbeef", "wait_seconds": 30 }),
            )
            .await
            .expect("the pipeline appears and finishes");
        assert!(text.starts_with("demo/dev #1 succeeded"), "{text}");
        let find = CiRequest::Find {
            repo: "demo".to_owned(),
            sha: "deadbeef".to_owned(),
        };
        assert_eq!(
            *ci.seen.asked.lock().unwrap(),
            [
                find.clone(),
                find.clone(),
                find,
                CiRequest::Pipeline {
                    repo: "demo".to_owned(),
                    number: 1
                }
            ]
        );

        // Nothing matches: the end of the wait says so.
        let text = tools
            .call(
                "ci_wait",
                json!({ "repo": "demo", "sha": "beef", "wait_seconds": 0 }),
            )
            .await
            .expect("a missing pipeline is waited for");
        assert_eq!(
            text,
            "no pipeline of beef in demo after 0 s; the push may not have reached the CI \
             yet; call ci_wait again\n"
        );
    }

    #[tokio::test]
    async fn ci_wait_wants_a_number_or_a_sha_and_nothing_else() {
        let tools = toolbox_without_a_ci();
        for arguments in [
            json!({ "repo": "r" }),
            json!({ "repo": "r", "number": 1, "sha": "abcd" }),
        ] {
            let err = tools.call("ci_wait", arguments).await.expect_err("both");
            assert_eq!(err, "give either number or sha");
        }
        for sha in ["abc", "xyz1", ""] {
            let err = tools
                .call("ci_wait", json!({ "repo": "r", "sha": sha }))
                .await
                .expect_err(sha);
            assert!(err.contains("is not 4 to 40 hex digits"), "{sha}: {err}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_timing_tells_the_pipeline_or_asks_the_ci_for_the_pauses_of_a_job() {
        let ci = fake_ci("timing", 0, None).await;
        let tools = toolbox_for(&ci);

        // Without a job it is the pipeline's own record, one look at it.
        let text = tools
            .call("ci_timing", json!({ "repo": "demo", "number": 1 }))
            .await
            .expect("the pipeline");
        assert!(
            text.starts_with(
                "demo/dev #1 succeeded manual main 0123456 cargo test (hannes@tuco-pc)\n\
                 created 00:16:40 UTC, took 7 s\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("started +0 s ended +12 s staging - ran 12 s\n"),
            "{text}"
        );
        assert_eq!(ci.seen.asked.lock().unwrap().len(), 1);

        // With one the CI is asked, for 15 lines unless told otherwise and
        // for no more than it gives.
        let timing = |extra: Value| {
            let mut arguments = json!({ "repo": "demo", "number": 1, "job": "build" });
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            tools.call("ci_timing", arguments)
        };
        let text = timing(json!({})).await.expect("the job's pauses");
        assert_eq!(
            text,
            "demo #1 build: 3 lines over 31.2 s of output, still running\n\
             (only the last 64 MiB of output were read)\n\
             the 1 lines followed by the longest pauses, in output order:\n\
             \x20 +0.0 s  30.0 s  build: asked for 15\n"
        );
        assert_eq!(
            ci.seen.asked.lock().unwrap().last(),
            Some(&CiRequest::Timing {
                repo: "demo".to_owned(),
                number: 1,
                job: "build".to_owned(),
                top: 15,
            })
        );
        let text = timing(json!({ "top": 1000 })).await.expect("clamped");
        assert!(text.contains("build: asked for 100\n"), "{text}");
        let text = timing(json!({ "top": 0 })).await.expect("clamped");
        assert!(text.contains("build: asked for 1\n"), "{text}");

        // The CI's refusal is the tool's error.
        let err = tools
            .call(
                "ci_timing",
                json!({ "repo": "demo", "number": 1, "job": "nothing" }),
            )
            .await
            .expect_err("no output");
        assert_eq!(err, "no output for nothing of #1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ci_log_asks_for_timestamps_only_when_told_to() {
        let ci = fake_ci("stamps", 0, None).await;
        let tools = toolbox_for(&ci);
        let log = |extra: Value| {
            let mut arguments = json!({ "repo": "demo", "number": 1, "job": "build" });
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            tools.call("ci_log", arguments)
        };

        let plain = log(json!({})).await.expect("log");
        assert_eq!(plain, "job build, 200 lines\ntest result: ok. 1 passed\n");
        assert!(matches!(
            ci.seen.asked.lock().unwrap().last(),
            Some(CiRequest::Log {
                timestamps: false,
                tail: 200,
                ..
            })
        ));

        let stamped = log(json!({ "timestamps": true })).await.expect("log");
        assert_eq!(
            stamped,
            "[+     0.0s] job build, 200 lines\ntest result: ok. 1 passed\n"
        );
        assert!(matches!(
            ci.seen.asked.lock().unwrap().last(),
            Some(CiRequest::Log {
                timestamps: true,
                ..
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_cis_refusal_of_a_run_is_the_tools_error() {
        let ci = fake_ci(
            "refuse",
            0,
            Some("git@example.invalid:o/demo.git is no repository of ci.toml"),
        )
        .await;
        let tools = toolbox_for(&ci);
        let dir = checkout("refuse");
        let err = tools
            .call("run_jobs", json!({ "jobs": ["build-x"], "path": dir }))
            .await
            .expect_err("refused");
        assert_eq!(
            err,
            "git@example.invalid:o/demo.git is no repository of ci.toml"
        );
        assert!(ci.seen.uploads.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A CI that refuses an upload does not read its bundles, which breaks
    /// the stream under a writer that still has megabytes to send. What the
    /// caller is told is the refusal, not the broken stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refusal_that_comes_before_the_bundles_are_read_is_still_heard() {
        const SIZE: usize = 48 << 20;
        let ci = fake_ci("early", 0, Some("no repository matches x")).await;
        let link = super::super::client::Link::resolve(Some(ci.url.clone()), &ci.state);
        let client = Client::new(link);
        let bundle = std::env::temp_dir().join(format!("zeughaus-mcp-big-{}", std::process::id()));
        std::fs::write(&bundle, vec![7u8; SIZE]).unwrap();
        let upload = RunUpload {
            origin: "x".to_owned(),
            actor: "a@b".to_owned(),
            summary: "s".to_owned(),
            jobs: vec!["build-x".to_owned()],
            dirs: vec![UploadDir {
                dir: "x".to_owned(),
                sha: "0".repeat(40),
            }],
            sizes: vec![SIZE as u64],
        };

        let reply = client
            .manual_upload(&upload, std::slice::from_ref(&bundle))
            .await
            .expect("a refusal is an answer");

        assert_eq!(
            reply,
            RunReply::Refused {
                message: "no repository matches x".to_owned()
            }
        );
        let _ = std::fs::remove_file(&bundle);
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_read_tools_answer_from_the_cis_records() {
        let ci = fake_ci("read", 0, None).await;
        let tools = toolbox_for(&ci);

        let text = tools
            .call("ci_pipeline", json!({ "repo": "demo", "number": 1 }))
            .await
            .expect("pipeline");
        assert!(
            text.starts_with("demo/dev #1 succeeded manual main 0123456"),
            "{text}"
        );
        assert!(text.contains("\n  build succeeded agent\n"), "{text}");

        let log = tools
            .call(
                "ci_log",
                json!({ "repo": "demo", "number": 1, "job": "build", "tail": 9_999 }),
            )
            .await
            .expect("log");
        // Asked for more than the CI gives: asked for what it gives.
        assert_eq!(log, "job build, 5000 lines\ntest result: ok. 1 passed\n");

        let err = tools
            .call("ci_rerun", json!({ "repo": "demo", "number": 1 }))
            .await
            .expect_err("the fake refuses");
        assert_eq!(err, "not served by the fake");
    }
}
