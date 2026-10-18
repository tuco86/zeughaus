//! The `/ci` service: what editors ask the CI about its pipelines (see
//! [`zeughaus_link::ci`]).
//!
//! Channels and pipelines are read from the records on disk, which the
//! scheduler writes before it acts on a change: a request never waits for
//! the scheduler, and a restarted scheduler changes nothing here. The
//! runners come from the registry the `/enlist` service keeps. A job's
//! transcript is a read-only screen in [`Transcripts`], served on `/mux`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use weida::{Replier, TransferMeta};
use zeughaus_link::ci::{MAX_PIPELINES_PER_REQUEST, MAX_TIMING_LINES, TIMING_READ_BYTES};
use zeughaus_link::enlist::Place;
use zeughaus_link::{
    BusyMode, ChannelView, CiOverview, CiReply, CiRequest, JobTiming, JobView, MAX_CI_REPLY_BYTES,
    MAX_CI_REQUEST_BYTES, MachineJob, MachineView, PipelineView, QueuedView, TimedLine,
    TransferView,
};
use zeughaus_mux::TerminalId;
use zeughaus_terminal::wal;

use crate::config::{CiConfig, valid_name};
use crate::event::{self, EventKind};
use crate::excerpt;
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

/// How much of a transcript's end a `Log` reply is made from.
const LOG_READ_BYTES: usize = 8 << 20;

/// The longest line of output a `Timing` reply carries, in characters.
const TIMING_TEXT_CHARS: usize = 200;

pub struct CiServer {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    runners: Arc<Runners>,
    transcripts: Arc<Transcripts>,
}

impl CiServer {
    pub fn new(
        state_dir: PathBuf,
        config: Arc<CiConfig>,
        runners: Arc<Runners>,
        transcripts: Arc<Transcripts>,
    ) -> CiServer {
        CiServer {
            state_dir,
            config,
            runners,
            transcripts,
        }
    }

    /// Blocking: reads records from disk.
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
            } => self.log(&repo, number, &job, tail, timestamps),
            CiRequest::Timing {
                repo,
                number,
                job,
                top,
            } => self.timing(&repo, number, &job, top),
        }
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
            platforms: self
                .config
                .platforms
                .iter()
                .map(|(platform, target)| (platform.clone(), target.clone()))
                .collect(),
        }
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
        if !(4..=40).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return error("sha must be 4 to 40 hex digits");
        }
        let sha = sha.to_ascii_lowercase();
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
            .find(|pipeline| {
                pipeline
                    .sha
                    .as_deref()
                    .is_some_and(|full| full.to_ascii_lowercase().starts_with(&sha))
            });
        CiReply::Pipeline {
            pipeline: found.as_ref().map(|record| Box::new(pipeline_view(record))),
        }
    }

    /// The end of a job's stored output as plain text: at most `tail` lines,
    /// and never more than a reply may carry. The transcript is read from
    /// its end, so a job that wrote for hours costs what its last lines do.
    /// With `timestamps` every line starts with when it began.
    fn log(&self, repo: &str, number: u64, job: &str, tail: u32, timestamps: bool) -> CiReply {
        if !valid_name(repo) || !valid_name(job) {
            return error("no such job");
        }
        let dir = transcript_dir(&self.state_dir, repo, number, job);
        if !dir.join("wal").exists() {
            return error(&format!("no output for {job} of #{number}"));
        }
        let lines = tail.min(MAX_LOG_LINES) as usize;
        let text = if timestamps {
            timestamped_tail(&dir, LOG_READ_BYTES, lines)
        } else {
            excerpt::read_log_tail(&dir, LOG_READ_BYTES)
        };
        match text {
            Ok(text) => CiReply::Log {
                text: last_lines(&text, lines, MAX_LOG_TEXT_BYTES).to_owned(),
                finished: dir.join("exit").exists(),
            },
            Err(e) => error(&e),
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

/// The last `lines` lines of `text`, cut from the front to at most `max`
/// bytes at the start of a line (inside the line when it alone is longer).
fn last_lines(text: &str, lines: usize, max: usize) -> &str {
    if lines == 0 {
        return "";
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    let start = body
        .match_indices('\n')
        .rev()
        .nth(lines - 1)
        .map_or(0, |(newline, _)| newline + 1);
    let kept = &text[start..];
    let Some(mut cut) = kept.len().checked_sub(max).filter(|&cut| cut > 0) else {
        return kept;
    };
    while !kept.is_char_boundary(cut) {
        cut += 1;
    }
    if kept.as_bytes()[cut - 1] == b'\n' {
        return &kept[cut..];
    }
    match kept[cut..].find('\n') {
        Some(newline) => &kept[cut + newline + 1..],
        None => &kept[cut..],
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

/// The last `keep` lines of the transcript in `dir` that its last `max`
/// bytes hold, each as `[+<seconds since the job's first output>s] <text>`.
fn timestamped_tail(dir: &Path, max: usize, keep: usize) -> Result<String, String> {
    let tail = timed_tail(dir, max)?;
    let origin = tail.origin.unwrap_or(0);
    let from = tail.lines.len().saturating_sub(keep);
    let mut text = String::new();
    for (at, line) in &tail.lines[from..] {
        let seconds = at.saturating_sub(origin) as f64 / 1e6;
        text.push_str(&format!("[+{seconds:>8.1}s] {line}\n"));
    }
    Ok(text)
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

    use zeughaus_link::MachineState;
    use zeughaus_link::enlist::{Offer, Place};

    use super::*;
    use crate::event::{CiEvent, EventKind};
    use crate::header::JobDef;
    use crate::runners::Connected;
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

    fn config() -> Arc<CiConfig> {
        let other = FINGERPRINT.replace('1', "2");
        Arc::new(
            CiConfig::parse(&format!(
                "default_runner = \"alpha\"\n\
                 [runners.alpha]\nfingerprint = \"{FINGERPRINT}\"\nmachines = [\"win11\"]\n\
                 [runners.beta]\nfingerprint = \"{other}\"\n\
                 [platforms]\nlinux-amd64 = \"alpha\"\nwindows-amd64 = \"win11\"\n"
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

    fn store(state: &Path, pipeline: &Pipeline) {
        let path = state
            .join("ci/pipelines")
            .join(&pipeline.repo)
            .join(format!("{}.json", pipeline.number));
        crate::files::write_json(&path, pipeline).expect("write the record");
    }

    fn server(state: &Path) -> CiServer {
        CiServer::new(
            state.to_path_buf(),
            config(),
            Arc::new(Runners::new()),
            Arc::new(Transcripts::new(state.to_path_buf())),
        )
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

    fn open(server: &CiServer, repo: &str, number: u64, job: &str) -> CiReply {
        server.answer(CiRequest::OpenTranscript {
            repo: repo.to_owned(),
            number,
            job: job.to_owned(),
        })
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
            let mut record = pipeline("view", number, channel, PipelineStatus::Running, Vec::new());
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
        let ask = |repo: &str, job: &str, tail| {
            server.answer(CiRequest::Log {
                repo: repo.to_owned(),
                number: 3,
                job: job.to_owned(),
                tail,
                timestamps: false,
            })
        };
        let log = |tail| match ask("view", "build", tail) {
            CiReply::Log { text, finished } => (text, finished),
            other => panic!("unexpected reply {other:?}"),
        };

        assert_eq!(ask("view", "build", 10), error("no output for build of #3"));
        let mut wal =
            zeughaus_terminal::wal::WalWriter::open(&dir.join("wal"), None).expect("open the WAL");
        wal.append(1, b"\x1b[32mone\x1b[0m\r\ntwo\r\n")
            .expect("append");
        wal.append(2, b"three\r\nfour\r\n").expect("append");

        // The escape sequences are gone, only the end is asked for, and the
        // job still runs.
        assert_eq!(log(2), ("three\nfour\n".to_owned(), false));
        assert_eq!(log(100), ("one\ntwo\nthree\nfour\n".to_owned(), false));
        assert_eq!(log(0), (String::new(), false));
        std::fs::write(dir.join("exit"), "code=0\n").expect("write the exit record");
        assert_eq!(log(1), ("four\n".to_owned(), true));

        // Names that are no job read nothing.
        assert_eq!(ask("../view", "build", 10), error("no such job"));
        assert_eq!(ask("view", "../build", 10), error("no such job"));
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
                "[+     0.0s] one\n[+     0.0s] slow\n[+     1.5s] two\n[+    65.8s] three\n"
                    .to_owned(),
                false
            )
        );
        assert_eq!(
            log(2),
            ("[+     1.5s] two\n[+    65.8s] three\n".to_owned(), false)
        );
        assert_eq!(log(0), (String::new(), false));
        std::fs::write(dir.join("exit"), "code=0\n").expect("write the exit record");
        assert_eq!(log(1), ("[+    65.8s] three\n".to_owned(), true));
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
                    at_ms: 200,
                    took_ms: 30_000,
                    text: "compiling a".to_owned()
                },
                TimedLine {
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
    fn the_last_lines_of_a_text_are_cut_at_a_line_start_within_the_byte_limit() {
        assert_eq!(last_lines("a\nb\nc\n", 2, 100), "b\nc\n");
        assert_eq!(last_lines("a\nb\nc", 2, 100), "b\nc");
        assert_eq!(last_lines("a\nb", 5, 100), "a\nb");
        assert_eq!(last_lines("a\nb\nc\n", 0, 100), "");
        assert_eq!(last_lines("", 3, 100), "");
        // Cut from the front to the next line start; inside a line when it
        // alone is longer, at a character.
        assert_eq!(last_lines("aaaa\nbb\ncc\n", 10, 6), "bb\ncc\n");
        assert_eq!(last_lines("aaaa\nbb\ncc\n", 10, 5), "cc\n");
        assert_eq!(last_lines("aaaa\nbb\ncc\n", 10, 100), "aaaa\nbb\ncc\n");
        assert_eq!(last_lines("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 1, 3), "\u{e9}");
    }
}
