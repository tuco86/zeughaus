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
use zeughaus_link::ci::MAX_PIPELINES_PER_REQUEST;
use zeughaus_link::tryrun::TryReply;
use zeughaus_link::{CiOverview, CiReply, CiRequest, JobView, PipelineView};

use super::client::Client;
use super::rpc::Tools;
use super::snapshot;
use crate::names::valid_name;

const INSTRUCTIONS: &str = "zeughaus is this person's CI: its runners are the machines they own. \
try_run executes a command on another host's zeughaus runner (Windows, macOS or a Linux container) \
from the current working tree, with its uncommitted files and the sibling checkouts its Cargo path \
dependencies need, and answers with the result and the end of the output; ci_status lists the \
targets it accepts, and with the other ci_* tools the CI's channels, pipelines and logs; ci_wait \
waits for a pipeline, or one job of it, to settle, also for the pipeline a push is about to start.";

/// Pipelines listed when the call gives no limit.
const DEFAULT_PIPELINES: u32 = 10;
/// Lines of a job's output returned when the call gives no count.
const DEFAULT_TAIL: u32 = 200;
/// The most lines of output one call returns; the CI cuts there too.
const MAX_TAIL: u32 = 5000;
/// Lines of output the report of a try run ends with.
const REPORT_LINES: u32 = 200;
const DEFAULT_TIMEOUT_MINUTES: u32 = 60;
const MAX_TIMEOUT_MINUTES: u32 = 1440;
/// How long a call waits for a try run when it is not told.
const DEFAULT_WAIT_SECONDS: u64 = 540;
/// The most a call waits: the client's own timeout for a tool call is 660 s.
const MAX_WAIT_SECONDS: u64 = 600;
/// Pause between two looks at a running pipeline.
const POLL: Duration = Duration::from_secs(2);
/// How long a pipeline that was just accepted may be missing from the CI's
/// records.
const RECORD_GRACE: Duration = Duration::from_secs(10);
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
        };
        match self.client.ci(request).await? {
            CiReply::Log { text, finished } => Ok(log_text(&text, finished)),
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

    async fn try_run(&self, args: TryRunArgs) -> Result<String, String> {
        let started = Instant::now();
        if !valid_name(&args.target) {
            return Err(format!(
                "target {:?} is not a platform or runner name; ci_status lists them",
                args.target
            ));
        }
        if let Some(image) = &args.image
            && !valid_name(image)
        {
            return Err(format!(
                "image {image:?} is not an image name: .zeughaus-ci/<image>.Containerfile"
            ));
        }
        let timeout_minutes = args.timeout_minutes.unwrap_or(DEFAULT_TIMEOUT_MINUTES);
        if !(1..=MAX_TIMEOUT_MINUTES).contains(&timeout_minutes) {
            return Err(format!(
                "timeout_minutes is {timeout_minutes}; it must be 1 to {MAX_TIMEOUT_MINUTES}"
            ));
        }
        if args.command.trim().is_empty() {
            return Err("command is empty".to_owned());
        }
        let wait = wait_for(args.wait_seconds);
        // The snapshot and its upload cost time and bytes: do not make them
        // for a CI that is not there.
        self.client.configured()?;

        let path = checkout_path(args.path.as_deref())?;
        let command = args.command.clone();
        let snapshot = tokio::task::spawn_blocking(move || snapshot::take(&path, &command))
            .await
            .map_err(|e| format!("the snapshot failed: {e}"))??;
        let upload = snapshot.upload(&args.target, args.image.as_deref(), timeout_minutes);
        let reply = self.client.try_upload(&upload, &snapshot.bundles).await?;
        // The bundles are on the CI now.
        drop(snapshot);
        match reply {
            TryReply::Accepted { repo, number } => {
                self.wait(
                    &repo,
                    number,
                    Some(&args.target),
                    started,
                    wait,
                    RECORD_GRACE,
                )
                .await
            }
            TryReply::Refused { message } => Err(message),
        }
    }

    async fn try_wait(&self, args: TryWaitArgs) -> Result<String, String> {
        let wait = wait_for(args.wait_seconds);
        self.wait(
            &args.repo,
            args.number,
            None,
            Instant::now(),
            wait,
            Duration::ZERO,
        )
        .await
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

    /// Looks at the pipeline until it has finished or `wait` has passed since
    /// `started`, and says where it stands. A pipeline missing from the CI's
    /// records is an error once `grace` has passed, and so is a CI that has
    /// not answered for the length of `error_grace`.
    async fn wait(
        &self,
        repo: &str,
        number: u64,
        target: Option<&str>,
        started: Instant,
        wait: Duration,
        grace: Duration,
    ) -> Result<String, String> {
        self.client.configured()?;
        let deadline = started + wait;
        let missing_until = Instant::now() + grace;
        let mut latest: Option<PipelineView> = None;
        let mut last_error = None;
        let mut failing_since: Option<Instant> = None;
        loop {
            match self.look(repo, number).await {
                Ok(Some(view)) if view.finished.is_some() => {
                    return Ok(self.report(&view, target).await);
                }
                Ok(Some(view)) => {
                    latest = Some(view);
                    last_error = None;
                    failing_since = None;
                }
                Ok(None) if Instant::now() >= missing_until => {
                    return Err(format!("no pipeline {repo} #{number}"));
                }
                Ok(None) => failing_since = None,
                // A CI that restarts or a link that drops is not the end of
                // the run: look again for a while.
                Err(e) => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= self.error_grace {
                        return Err(unreachable_text(
                            &e,
                            &format!("{repo} #{number}"),
                            "try_wait",
                        ));
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
        match (latest, last_error) {
            (Some(view), _) => Ok(running_text(
                &view,
                target,
                now(),
                started.elapsed().as_secs(),
            )),
            (None, Some(e)) => Err(unreachable_text(
                &e,
                &format!("{repo} #{number}"),
                "try_wait",
            )),
            (None, None) => Err(format!("no pipeline {repo} #{number}")),
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

    /// The result of a finished try run: how it ended and the end of its
    /// output.
    async fn report(&self, view: &PipelineView, target: Option<&str>) -> String {
        let log = match try_job(view) {
            Some(job) => {
                let request = CiRequest::Log {
                    repo: view.repo.clone(),
                    number: view.number,
                    job: job.name.clone(),
                    tail: REPORT_LINES,
                };
                Some(self.client.ci(request).await)
            }
            None => None,
        };
        report_text(view, target, now(), log)
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
            "ci_rerun" => self.rerun(arguments_of(arguments)?).await,
            "ci_cancel" => self.cancel(arguments_of(arguments)?).await,
            "ci_wait" => self.ci_wait(arguments_of(arguments)?).await,
            "try_run" => self.try_run(arguments_of(arguments)?).await,
            "try_wait" => self.try_wait(arguments_of(arguments)?).await,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TryRunArgs {
    target: String,
    command: String,
    path: Option<String>,
    image: Option<String>,
    timeout_minutes: Option<u32>,
    wait_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TryWaitArgs {
    repo: String,
    number: u64,
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

/// The directory a try run is taken from: the call's `path`, else the
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

/// `ci_pipeline`: the header, then every job; a failed job adds why.
fn pipeline_text(pipeline: &PipelineView) -> String {
    let mut text = format!(
        "{}/{} {}\n",
        pipeline.repo,
        pipeline.channel,
        pipeline_line(pipeline)
    );
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

/// The job a try run is about: the one named `try`, else the first.
fn try_job(pipeline: &PipelineView) -> Option<&JobView> {
    pipeline
        .jobs
        .iter()
        .find(|job| job.name == "try")
        .or_else(|| pipeline.jobs.first())
}

/// `<repo> #<n> try on <target>: <status> (<secs> s)`. `target` is what the
/// caller asked for; without it the place of the job says where it runs.
fn header(pipeline: &PipelineView, target: Option<&str>, now: u64) -> String {
    let seconds = pipeline
        .finished
        .unwrap_or(now)
        .saturating_sub(pipeline.created);
    let place = target
        .map(str::to_owned)
        .or_else(|| try_job(pipeline).map(|job| job.place.clone()))
        .filter(|place| !place.is_empty());
    let on = place.map_or_else(String::new, |place| format!(" on {place}"));
    format!(
        "{} #{} try{on}: {} ({seconds} s)",
        pipeline.repo, pipeline.number, pipeline.status
    )
}

/// A finished try run: the header, what the CI says about it, and the end
/// of its output as `log` returned it (`None` when it has no job).
fn report_text(
    pipeline: &PipelineView,
    target: Option<&str>,
    now: u64,
    log: Option<Result<CiReply, String>>,
) -> String {
    let mut text = header(pipeline, target, now);
    text.push('\n');
    if !pipeline.note.is_empty() {
        text.push_str(&format!("note: {}\n", one_line(&pipeline.note)));
    }
    if let Some(job) = try_job(pipeline) {
        let mut facts = Vec::new();
        if let Some(code) = job.code.filter(|code| *code != 0) {
            facts.push(format!("exit code {code}"));
        }
        let note = one_line(&job.note);
        if !note.is_empty() {
            facts.push(note);
        }
        if !facts.is_empty() {
            text.push_str(&format!("{}: {}\n", job.name, facts.join("; ")));
        }
        if let Some(cause) = &job.cause {
            text.push_str(&format!("cause: {}\n", one_line(cause)));
        }
    }
    match log {
        Some(Ok(CiReply::Log { text: output, .. })) => {
            text.push_str(&format!("--- last {REPORT_LINES} lines ---\n"));
            text.push_str(output.trim_end_matches('\n'));
            text.push('\n');
        }
        Some(Ok(other)) => text.push_str(&format!("--- no output: {} ---\n", unexpected(&other))),
        Some(Err(e)) => text.push_str(&format!("--- no output: {e} ---\n")),
        None => {}
    }
    text
}

/// A try run that has not finished when the call stops waiting.
fn running_text(
    pipeline: &PipelineView,
    target: Option<&str>,
    now: u64,
    waited_seconds: u64,
) -> String {
    let mut text = header(pipeline, target, now);
    text.push('\n');
    for job in &pipeline.jobs {
        text.push_str(&job_line(job));
        text.push('\n');
    }
    text.push_str(&format!(
        "still running after {waited_seconds} s; call try_wait with repo={} number={}\n",
        pipeline.repo, pipeline.number
    ));
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
             queue, and the platforms (name -> runner or machine) that try_run targets resolve \
             through. Start here.",
            json!({}),
            &[],
            true,
        ),
        tool(
            "ci_pipelines",
            "A channel's pipelines, newest first, one line each: #number status event ref \
             sha subject (actor). Try runs are in the channel `try`.",
            json!({
                "repo": repo,
                "channel": {
                    "type": "string",
                    "description": "Channel name as ci_status lists it, for example dev, \
                                    nightly, release or try.",
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
                    "description": "Job name as ci_pipeline lists it. The job of a try run \
                                    is `try`.",
                },
                "tail": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TAIL,
                    "default": DEFAULT_TAIL,
                    "description": "How many lines from the end.",
                },
            }),
            &["repo", "number", "job"],
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
             `accepted`. A try run cannot be rerun: start a new one with try_run.",
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
            "try_run",
            "Run a command on another host's zeughaus runner from the current working tree. \
             The checkout at `path` is snapshotted as it is on disk (its commit, the \
             changes to it, and untracked files that are not ignored), together with the \
             sibling checkouts its Cargo path dependencies point to (../name), uploaded to the \
             CI and run there as a pipeline of the channel `try`. Tries of one repository run \
             one after another on a runner, and build output stays warm between them. \
             Answers with `<repo> #<n> try on <target>: <status> (<secs> s)` and the last 200 \
             lines of output, or, when the run is still going after wait_seconds, how to \
             continue with try_wait.",
            json!({
                "target": {
                    "type": "string",
                    "description": "Where to run: a platform from ci_status (for example \
                                    linux-arm64, darwin-arm64, windows-amd64) or a runner \
                                    name (for example atik, wohnzimmer).",
                },
                "command": {
                    "type": "string",
                    "description": "The command line: sh on Unix runners and in containers, \
                                    PowerShell on Windows runners. It runs in the directory \
                                    of the snapshot that `path` is in.",
                },
                "path": {
                    "type": "string",
                    "description": "A directory inside the checkout to run from. Default: \
                                    the working directory of this server.",
                },
                "image": {
                    "type": "string",
                    "description": "Run in a container built from \
                                    .zeughaus-ci/<image>.Containerfile of the snapshot \
                                    instead of on the runner's host.",
                },
                "timeout_minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MINUTES,
                    "default": DEFAULT_TIMEOUT_MINUTES,
                    "description": "The run is killed after this long.",
                },
                "wait_seconds": wait(
                    "How long this call may take in all, snapshot and upload included. A \
                     run that has not finished by then is picked up with try_wait."
                ),
            }),
            &["target", "command"],
            false,
        ),
        tool(
            "try_wait",
            "Wait for a try run started by try_run, which names its repo and number, and \
             answer as try_run does.",
            json!({
                "repo": repo,
                "number": number,
                "wait_seconds": wait("How long to wait for the run to finish."),
            }),
            &["repo", "number"],
            true,
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
    use zeughaus_link::tryrun::TryUpload;
    use zeughaus_link::{CI_PATH, ChannelView, MachineJob, MachineView, QueuedView, TRY_PATH};

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
            channel: "try".to_owned(),
            event: "try".to_owned(),
            git_ref: "try".to_owned(),
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
                "ci_wait",
                "ci_rerun",
                "ci_cancel",
                "try_run",
                "try_wait"
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
        let run = &definitions[7]["inputSchema"];
        assert_eq!(run["required"], json!(["target", "command"]));
        assert_eq!(run["properties"]["wait_seconds"]["default"], 540);
        assert_eq!(run["properties"]["wait_seconds"]["maximum"], 600);
        assert_eq!(run["properties"]["timeout_minutes"]["default"], 60);
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
                    channel: "try".to_owned(),
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
                        channel: "try".to_owned(),
                        number: 9,
                        job: "try".to_owned(),
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
            "channels\n  zeughaus/dev #123 succeeded\n  zeughaus/try #9 running\n\
             machines\n  tuco-pc runner free [zeughaus#9/try]\n  win11 windows-vm off\n\
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
    fn a_finished_try_run_reports_how_it_ended_and_the_end_of_its_output() {
        let log = |text: &str| {
            Some(Ok(CiReply::Log {
                text: text.to_owned(),
                finished: true,
            }))
        };
        let ok = pipeline(
            "succeeded",
            Some(1_012),
            vec![job("try", "succeeded", "agent", "")],
        );
        assert_eq!(
            report_text(
                &ok,
                Some("agent"),
                5_000,
                log("test result: ok. 1 passed\n")
            ),
            "demo #7 try on agent: succeeded (12 s)\n\
             --- last 200 lines ---\n\
             test result: ok. 1 passed\n"
        );

        let mut failed_job = job("try", "failed", "wohnzimmer", "exited with 101");
        failed_job.code = Some(101);
        failed_job.cause = Some("thread 'x' panicked at src/lib.rs:3".to_owned());
        let failed = pipeline("failed", Some(1_042), vec![failed_job]);
        // Without the target of the call, the job's place says where.
        assert_eq!(
            report_text(&failed, None, 5_000, log("a\nb")),
            "demo #7 try on wohnzimmer: failed (42 s)\n\
             try: exit code 101; exited with 101\n\
             cause: thread 'x' panicked at src/lib.rs:3\n\
             --- last 200 lines ---\n\
             a\nb\n"
        );

        // A pipeline that never got a job, and a log that cannot be had.
        let mut refused = pipeline("error", Some(1_001), Vec::new());
        refused.note = "try runs on a runner's host or in a container".to_owned();
        assert_eq!(
            report_text(&refused, Some("win11"), 5_000, None),
            "demo #7 try on win11: error (1 s)\n\
             note: try runs on a runner's host or in a container\n"
        );
        assert_eq!(
            report_text(
                &ok,
                None,
                5_000,
                Some(Err("no output for try of #7".to_owned()))
            ),
            "demo #7 try on agent: succeeded (12 s)\n\
             --- no output: no output for try of #7 ---\n"
        );
    }

    #[test]
    fn a_try_run_that_is_still_going_says_how_to_continue() {
        let running = pipeline(
            "running",
            None,
            vec![job(
                "try",
                "waiting",
                "agent",
                "context zeughaus-try held by #6",
            )],
        );
        assert_eq!(
            running_text(&running, Some("agent"), 1_541, 541),
            "demo #7 try on agent: running (541 s)\n\
             \x20 try waiting agent context zeughaus-try held by #6\n\
             still running after 541 s; call try_wait with repo=demo number=7\n"
        );
    }

    #[test]
    fn a_log_says_when_the_job_still_runs() {
        assert_eq!(log_text("a\nb\n\n", true), "a\nb\n");
        assert_eq!(
            log_text("a", false),
            "a\n(the job still runs; more output may follow)\n"
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
            ("ci_rerun", json!({ "repo": "r", "number": 1 })),
            ("ci_cancel", json!({ "repo": "r", "number": 1 })),
            ("ci_wait", json!({ "repo": "r", "number": 1 })),
            ("try_run", json!({ "target": "t", "command": "true" })),
            ("try_wait", json!({ "repo": "r", "number": 1 })),
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
        assert!(unknown.contains("try_run"), "{unknown}");

        for (arguments, expected) in [
            (json!({ "command": "true" }), "missing field `target`"),
            (
                json!({ "target": "t", "command": "true", "wait": 1 }),
                "unknown field `wait`",
            ),
            (
                json!({ "target": "t", "command": "true", "timeout_minutes": "5" }),
                "invalid type",
            ),
            (
                json!({ "target": "Windows 10", "command": "true" }),
                "is not a platform or runner name",
            ),
            (
                json!({ "target": "t", "command": "true", "image": "a/b" }),
                "is not an image name",
            ),
            (
                json!({ "target": "t", "command": "true", "timeout_minutes": 0 }),
                "it must be 1 to 1440",
            ),
            (
                json!({ "target": "t", "command": "true", "timeout_minutes": 1441 }),
                "it must be 1 to 1440",
            ),
            (
                json!({ "target": "t", "command": "  \n" }),
                "command is empty",
            ),
        ] {
            let err = tools
                .call("try_run", arguments.clone())
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
        uploads: Mutex<Vec<(TryUpload, Vec<Vec<u8>>)>>,
        asked: Mutex<Vec<CiRequest>>,
        /// Looks at the pipeline before it is finished.
        running_looks: AtomicU32,
        /// Asks to find a pipeline by sha.
        finds: AtomicU32,
    }

    /// A weida listener that answers `/ci` and `/try` the way the CI does,
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
        let try_run = transport.listener().replier(TRY_PATH).expect("/try");
        tokio::spawn(serve_try(
            try_run,
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
            pipeline("running", None, vec![job("try", "running", "agent", "")])
        } else {
            let mut done = job("try", "succeeded", "agent", "");
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
                CiRequest::Log { job, tail, .. } => CiReply::Log {
                    text: format!("job {job}, {tail} lines\ntest result: ok. 1 passed\n"),
                    finished: true,
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

    async fn serve_try(replier: weida::Replier, seen: Arc<Seen>, refuse: Option<String>) {
        while let Ok(mut request) = replier.accept().await {
            let mut body = request.take_body();
            let upload: TryUpload = read_frame(&mut body)
                .await
                .expect("frame")
                .expect("an upload");
            let reply = match &refuse {
                Some(message) => TryReply::Refused {
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
                    TryReply::Accepted {
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

    /// The whole of a try run against a CI that is not the real one: the
    /// snapshot is taken from a directory, uploaded, and the pipeline looked
    /// at until it has ended, then its output is fetched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_try_run_uploads_the_snapshot_and_reports_the_pipeline() {
        let ci = fake_ci("run", 2, None).await;
        let tools = toolbox_for(&ci);
        let dir = checkout("run");

        let text = tools
            .call(
                "try_run",
                json!({
                    "target": "agent",
                    "command": "test -f note.txt && cargo test",
                    "path": dir.join("sub"),
                    "timeout_minutes": 5,
                    "wait_seconds": 30,
                }),
            )
            .await
            .expect("the run");

        assert!(
            text.starts_with("demo #1 try on agent: succeeded (7 s)\n"),
            "{text}"
        );
        assert!(text.contains("--- last 200 lines ---\n"), "{text}");
        assert!(text.contains("test result: ok. 1 passed\n"), "{text}");

        let uploads = ci.seen.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        let (upload, bundles) = &uploads[0];
        assert_eq!(upload.origin, "git@example.invalid:o/demo.git");
        assert_eq!(upload.target, "agent");
        assert_eq!(upload.image, None);
        assert_eq!(upload.timeout_minutes, 5);
        assert_eq!(upload.summary, "test -f note.txt && cargo test");
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
        // The pipeline was looked at while it ran, then its log was asked for
        // as the job that ran.
        assert_eq!(ci.seen.running_looks.load(Ordering::Relaxed), 3);
        let asked = ci.seen.asked.lock().unwrap();
        assert!(matches!(
            asked.last(),
            Some(CiRequest::Log { job, tail: 200, .. }) if job == "try"
        ));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_try_run_that_outlasts_the_wait_can_be_waited_for_again() {
        let ci = fake_ci("again", 1_000, None).await;
        let tools = toolbox_for(&ci);
        let dir = checkout("again");

        let text = tools
            .call(
                "try_run",
                json!({
                    "target": "agent",
                    "command": "cargo test",
                    "path": dir,
                    "wait_seconds": 0,
                }),
            )
            .await
            .expect("still running is not an error");
        assert!(text.starts_with("demo #1 try on agent: running"), "{text}");
        assert!(
            text.ends_with("still running after 0 s; call try_wait with repo=demo number=1\n"),
            "{text}"
        );

        // try_wait with a pipeline the CI does not know is an error at once.
        let err = tools
            .call(
                "try_wait",
                json!({ "repo": "demo", "number": 2, "wait_seconds": 5 }),
            )
            .await
            .expect_err("unknown pipeline");
        assert_eq!(err, "no pipeline demo #2");
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
                "try_wait",
                json!({ "repo": "demo", "number": 3, "wait_seconds": 30 }),
            )
            .await
            .expect_err("the CI fails");
        assert_eq!(
            err,
            "the scheduler is restarting (the pipeline is demo #3; try_wait asks again)"
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
            text.starts_with("demo/try #1 succeeded try try 0123456"),
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
        assert!(text.starts_with("demo/try #1 succeeded"), "{text}");
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
    async fn the_cis_refusal_of_an_upload_is_the_tools_error() {
        let ci = fake_ci(
            "refuse",
            0,
            Some("no repository matches git@example.invalid:o/demo.git"),
        )
        .await;
        let tools = toolbox_for(&ci);
        let dir = checkout("refuse");
        let err = tools
            .call(
                "try_run",
                json!({ "target": "agent", "command": "true", "path": dir }),
            )
            .await
            .expect_err("refused");
        assert_eq!(err, "no repository matches git@example.invalid:o/demo.git");
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
        let upload = TryUpload {
            origin: "x".to_owned(),
            target: "agent".to_owned(),
            image: None,
            timeout_minutes: 1,
            actor: "a@b".to_owned(),
            summary: "s".to_owned(),
            dirs: vec![UploadDir {
                dir: "x".to_owned(),
                sha: "0".repeat(40),
            }],
            sizes: vec![SIZE as u64],
        };

        let reply = client
            .try_upload(&upload, std::slice::from_ref(&bundle))
            .await
            .expect("a refusal is an answer");

        assert_eq!(
            reply,
            TryReply::Refused {
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
            text.starts_with("demo/try #1 succeeded try try 0123456"),
            "{text}"
        );
        assert!(text.contains("\n  try succeeded agent\n"), "{text}");

        let log = tools
            .call(
                "ci_log",
                json!({ "repo": "demo", "number": 1, "job": "try", "tail": 9_999 }),
            )
            .await
            .expect("log");
        // Asked for more than the CI gives: asked for what it gives.
        assert_eq!(log, "job try, 5000 lines\ntest result: ok. 1 passed\n");

        let err = tools
            .call("ci_rerun", json!({ "repo": "demo", "number": 1 }))
            .await
            .expect_err("the fake refuses");
        assert_eq!(err, "not served by the fake");
    }
}
