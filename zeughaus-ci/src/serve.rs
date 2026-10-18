//! The `/ci` service: what editors ask the CI about its pipelines (see
//! [`zeughaus_link::ci`]).
//!
//! Channels and pipelines are read from the records on disk, which the
//! scheduler writes before it acts on a change: a request never waits for
//! the scheduler, and a restarted scheduler changes nothing here. The
//! runners come from the registry the `/enlist` service keeps. A job's
//! transcript is a read-only screen in [`Transcripts`], served on `/mux`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use weida::{Replier, TransferMeta};
use zeughaus_link::ci::MAX_PIPELINES_PER_REQUEST;
use zeughaus_link::enlist::Place;
use zeughaus_link::{
    BusyMode, ChannelView, CiOverview, CiReply, CiRequest, JobView, MAX_CI_REPLY_BYTES,
    MAX_CI_REQUEST_BYTES, MachineJob, MachineView, PipelineView, QueuedView, TransferView,
};
use zeughaus_mux::TerminalId;

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

/// How much of a transcript's end a `Log` reply is made from.
const LOG_READ_BYTES: usize = 8 << 20;

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
            CiRequest::Rerun { repo, number } => self.rerun(&repo, number),
            CiRequest::Cancel { repo, number } => self.cancel(&repo, number),
            CiRequest::Log {
                repo,
                number,
                job,
                tail,
            } => self.log(&repo, number, &job, tail),
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
        if pipeline.event.kind == EventKind::Try {
            return error("a try run cannot be rerun; start a new one");
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

    /// The end of a job's stored output as plain text: at most `tail` lines,
    /// and never more than a reply may carry. The transcript is read from
    /// its end, so a job that wrote for hours costs what its last lines do.
    fn log(&self, repo: &str, number: u64, job: &str, tail: u32) -> CiReply {
        if !valid_name(repo) || !valid_name(job) {
            return error("no such job");
        }
        let dir = transcript_dir(&self.state_dir, repo, number, job);
        if !dir.join("wal").exists() {
            return error(&format!("no output for {job} of #{number}"));
        }
        match excerpt::read_log_tail(&dir, LOG_READ_BYTES) {
            Ok(text) => CiReply::Log {
                text: last_lines(&text, tail.min(MAX_LOG_LINES) as usize, MAX_LOG_TEXT_BYTES)
                    .to_owned(),
                finished: dir.join("exit").exists(),
            },
            Err(e) => error(&e),
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
                context: None,
                cache: Vec::new(),
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
    fn a_try_run_cannot_be_rerun() {
        let state = scratch("rerun-try");
        let mut record = pipeline("view", 4, "try", PipelineStatus::Failed, Vec::new());
        record.event.kind = EventKind::Try;
        record.event.git_ref = "try".to_owned();
        record.event.upload = Some("0123456789abcdef".to_owned());
        store(&state, &record);
        let server = server(&state);
        assert_eq!(
            server.answer(CiRequest::Rerun {
                repo: "view".to_owned(),
                number: 4,
            }),
            error("a try run cannot be rerun; start a new one")
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
