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
    MAX_CI_REQUEST_BYTES, MachineJob, MachineView, PipelineView,
};
use zeughaus_mux::TerminalId;

use crate::config::{CiConfig, valid_name};
use crate::runners::Runners;
use crate::scheduler::{
    self, JobRecord, Pipeline, PipelineStatus, load_pipeline, load_pipelines, load_repo_pipelines,
    transcript_dir,
};
use crate::transcripts::Transcripts;
use crate::transport::editor_principal;

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
        // One pass over every record feeds both halves.
        let pipelines = load_pipelines(&self.state_dir);
        CiOverview {
            channels: channel_views(&pipelines),
            machines: self.machine_views(&pipelines),
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
}

fn error(message: &str) -> CiReply {
    CiReply::Error {
        message: message.to_owned(),
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
                 [runners.beta]\nfingerprint = \"{other}\"\n"
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
                received: 0,
            },
            sha: Some(format!("{number:040}")),
            subject: String::new(),
            status,
            note: String::new(),
            created: number,
            finished: None,
            jobs,
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
}
