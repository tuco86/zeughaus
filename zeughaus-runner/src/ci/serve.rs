//! The `/ci` service: what editors ask a runner that runs CI about its
//! pipelines (see [`zeughaus_link::ci`]).
//!
//! Channels and pipelines are read from the records on disk, which the
//! scheduler writes before it acts on a change: a request never waits for
//! the scheduler, and a restarted scheduler changes nothing here. Machines
//! and the busy state are read from the handles the scheduler shares. A
//! job's transcript is a read-only terminal in the runner's mux whose child
//! is `ci replay` (see [`super::replay`]); it is opened on request, and at
//! most [`MAX_TRANSCRIPTS`] stay open.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use weida::{Replier, TransferMeta};
use zeughaus_link::ci::MAX_PIPELINES_PER_REQUEST;
use zeughaus_link::{
    BusyMode, ChannelView, CiOverview, CiReply, CiRequest, JobView, MAX_CI_REPLY_BYTES,
    MAX_CI_REQUEST_BYTES, MachineView, PipelineView,
};
use zeughaus_mux::TerminalId;
use zeughaus_terminal::Profile;
use zeughaus_terminal::wal::Wal;

use super::busy::Busy;
use super::config::valid_name;
use super::machine::Machine;
use super::scheduler::{
    self, JobRecord, Pipeline, PipelineStatus, load_pipeline, load_pipelines, load_repo_pipelines,
    transcript_dir,
};
use crate::jobs::RUN_SCROLLBACK_ROWS;
use crate::mux::{MuxService, OwnedPlacement};

/// Transcript terminals kept open across all editors. Opening one more
/// closes the one used longest ago: a transcript is cheap to open again.
const MAX_TRANSCRIPTS: usize = 16;

/// A job: its repository, its pipeline's number and its name.
type JobKey = (String, u64, String);

/// A transcript terminal this server opened.
struct Open {
    key: JobKey,
    terminal: TerminalId,
}

pub struct CiServer {
    state_dir: PathBuf,
    machines: BTreeMap<String, Machine>,
    busy: Arc<Busy>,
    mux: MuxService,
    /// This binary, which a transcript's terminal runs as `ci replay`.
    exe: Option<PathBuf>,
    /// The transcripts open now, the one used longest ago first.
    transcripts: Mutex<Vec<Open>>,
}

impl CiServer {
    pub fn new(
        state_dir: PathBuf,
        machines: BTreeMap<String, Machine>,
        busy: Arc<Busy>,
        mux: MuxService,
        exe: Option<PathBuf>,
    ) -> CiServer {
        CiServer {
            state_dir,
            machines,
            busy,
            mux,
            exe,
            transcripts: Mutex::new(Vec::new()),
        }
    }

    /// Blocking: reads records from disk and may start a terminal.
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
                pipeline: self.record(&repo, number).as_ref().map(pipeline_view),
            },
            CiRequest::OpenTranscript { repo, number, job } => {
                self.open_transcript(&repo, number, &job)
            }
            CiRequest::CloseTranscript { terminal } => self.close_transcript(TerminalId(terminal)),
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

    /// The workstation first, then the configured machines by name, each
    /// with the jobs running on it.
    fn machine_views(&self, pipelines: &[Pipeline]) -> Vec<MachineView> {
        let active = active_jobs(pipelines);
        let running_on = |machine: Option<&str>| -> Vec<String> {
            active
                .iter()
                .filter(|(on, _)| on.as_deref() == machine)
                .map(|(_, label)| label.clone())
                .collect()
        };
        let state = self.busy.state();
        let mut status = if state.busy { "busy" } else { "free" }.to_owned();
        if state.mode != BusyMode::Auto {
            status.push_str(" (set)");
        }
        let mut views = vec![MachineView {
            name: "workstation".to_owned(),
            kind: "workstation".to_owned(),
            status,
            jobs: running_on(None),
        }];
        views.extend(self.machines.iter().map(|(name, machine)| MachineView {
            name: name.clone(),
            kind: machine.kind().to_owned(),
            status: machine.status().to_owned(),
            jobs: running_on(Some(name)),
        }));
        views
    }

    fn open_transcript(&self, repo: &str, number: u64, job: &str) -> CiReply {
        let Some(pipeline) = self.record(repo, number) else {
            return error("no such job");
        };
        let Some(record) = pipeline.jobs.iter().find(|j| j.def.name == job) else {
            return error("no such job");
        };
        let args = match replay_args(&self.state_dir, &pipeline, record) {
            Ok(args) => args,
            Err(message) => return CiReply::Error { message },
        };
        let key = (repo.to_owned(), number, job.to_owned());
        let mut open = self.transcripts.lock().unwrap_or_else(|e| e.into_inner());
        // A terminal someone else closed is no longer ours to count.
        open.retain(|o| self.mux.session(o.terminal).is_some());
        if let Some(at) = open.iter().position(|o| o.key == key) {
            let entry = open.remove(at);
            if self.replaying(entry.terminal) {
                let terminal = entry.terminal;
                open.push(entry);
                return CiReply::Transcript {
                    terminal: terminal.0,
                };
            }
            // The replay ended by itself, because it could not read its
            // file: a new one takes its place.
            let _ = self.mux.close_terminal(entry.terminal);
        }
        let Some(exe) = &self.exe else {
            return error("no runner executable to replay with");
        };
        if open.len() >= MAX_TRANSCRIPTS {
            let oldest = open.remove(0);
            let _ = self.mux.close_terminal(oldest.terminal);
        }
        let profile = Profile {
            label: format!("{repo} #{number} {job}"),
            program: Some(exe.clone()),
            args,
            cwd: None,
            env: Vec::new(),
            scrollback_rows: RUN_SCROLLBACK_ROWS,
        };
        match self
            .mux
            .spawn_owned(profile, Wal::Off, None, OwnedPlacement::Transcript)
        {
            Ok(terminal) => {
                open.push(Open { key, terminal });
                CiReply::Transcript {
                    terminal: terminal.0,
                }
            }
            Err(message) => CiReply::Error { message },
        }
    }

    /// Whether the terminal's replay is still there: a replay that ended
    /// has left a screen nobody can add to.
    fn replaying(&self, terminal: TerminalId) -> bool {
        self.mux
            .session(terminal)
            .is_some_and(|session| session.exit().is_none())
    }

    /// Closes a transcript this server opened. One it does not know -- closed
    /// already, by another editor or by the cap -- is as closed as asked.
    fn close_transcript(&self, terminal: TerminalId) -> CiReply {
        let mut open = self.transcripts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = open.iter().position(|o| o.terminal == terminal) {
            open.remove(at);
            let _ = self.mux.close_terminal(terminal);
        }
        CiReply::Closed
    }
}

fn error(message: &str) -> CiReply {
    CiReply::Error {
        message: message.to_owned(),
    }
}

/// The arguments after `ci replay` that show `record`'s output: the kept
/// transcript, else the run's own WAL, else the plain log of a run from
/// before the WAL. A job still running is followed until its run records its
/// exit, which a plain log cannot be.
fn replay_args(
    state_dir: &Path,
    pipeline: &Pipeline,
    record: &JobRecord,
) -> Result<Vec<String>, String> {
    let name = &record.def.name;
    let run_dir = record.run_dir.as_deref();
    let kept = transcript_dir(state_dir, &pipeline.repo, pipeline.number, name).join("wal");
    let run_wal = run_dir.map(|dir| dir.join("wal"));
    let run_log = run_dir.map(|dir| dir.join("log"));
    let (file, raw) = if kept.exists() {
        (kept, false)
    } else if let Some(wal) = run_wal.filter(|wal| wal.exists()) {
        (wal, false)
    } else if let Some(log) = run_log.filter(|log| log.exists()) {
        (log, true)
    } else {
        return Err(format!("{name} has no transcript"));
    };
    let mut args = vec![
        "ci".to_owned(),
        "replay".to_owned(),
        file.to_string_lossy().into_owned(),
    ];
    if raw {
        args.push("--raw".to_owned());
    } else if let Some(run_dir) = run_dir.filter(|_| record.status.is_active()) {
        args.push("--until".to_owned());
        args.push(run_dir.join("exit").to_string_lossy().into_owned());
    }
    Ok(args)
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
        place: scheduler::place_name(&job.def),
        status: job.status.as_str().to_owned(),
        note: job.note.clone(),
        started: job.started,
        finished: job.finished,
        code: job.code,
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

/// The jobs running now as (the machine they run on, `<repo> #<n> <job>`);
/// no machine is the workstation itself.
fn active_jobs(pipelines: &[Pipeline]) -> Vec<(Option<String>, String)> {
    pipelines
        .iter()
        .filter(|p| p.status == PipelineStatus::Running)
        .flat_map(|p| {
            p.jobs
                .iter()
                .filter(|j| j.status.is_active())
                .map(move |j| {
                    (
                        j.def.machine.clone(),
                        format!("{} #{} {}", p.repo, p.number, j.def.name),
                    )
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
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::ci::busy;
    use crate::ci::config::{BusyConfig, MachineConfig, UnixHostConfig};
    use crate::ci::event::{CiEvent, EventKind};
    use crate::ci::header::{JobDef, WhenBusy};
    use crate::ci::scheduler::JobStatus;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeughaus-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    fn job(name: &str, status: JobStatus, machine: Option<&str>) -> JobRecord {
        JobRecord {
            def: JobDef {
                name: name.to_owned(),
                file: format!("{name}.sh"),
                on: Vec::new(),
                needs: Vec::new(),
                image: None,
                machine: machine.map(str::to_owned),
                cache: Vec::new(),
                secrets: Vec::new(),
                when_busy: WhenBusy::Wait,
                env: Default::default(),
                timeout_minutes: 60,
                interpreter: "sh".to_owned(),
            },
            status,
            note: String::new(),
            run_dir: None,
            code: None,
            started: None,
            finished: None,
            image_tag: None,
            active_secs: 0,
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

    fn server(state: &Path, machines: BTreeMap<String, Machine>, exe: Option<PathBuf>) -> CiServer {
        let mux = MuxService::new(
            crate::mux::incarnation(),
            Vec::new(),
            zeughaus_terminal::TerminalHost::Local,
        );
        let busy = busy::start(BusyConfig::default(), state);
        CiServer::new(state.to_path_buf(), machines, busy, mux, exe)
    }

    /// A program standing in for `zeughaus-runner ci replay`: `body` runs
    /// where the replay would.
    fn stub(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("replay-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make the stub executable");
        path
    }

    /// Pipeline 1 of `view`: a finished one whose jobs `j0..` each have a
    /// run directory with a WAL in it.
    fn with_transcripts(state: &Path, jobs: usize) {
        let records = (0..jobs)
            .map(|i| {
                let run_dir = state.join("runs").join(i.to_string());
                std::fs::create_dir_all(&run_dir).expect("create the run directory");
                std::fs::write(run_dir.join("wal"), "").expect("write the WAL");
                let mut record = job(&format!("j{i}"), JobStatus::Succeeded, None);
                record.run_dir = Some(run_dir);
                record
            })
            .collect();
        store(
            state,
            &pipeline("view", 1, "dev", PipelineStatus::Succeeded, records),
        );
    }

    fn open(server: &CiServer, job: &str) -> Result<u64, String> {
        match server.answer(CiRequest::OpenTranscript {
            repo: "view".to_owned(),
            number: 1,
            job: job.to_owned(),
        }) {
            CiReply::Transcript { terminal } => Ok(terminal),
            CiReply::Error { message } => Err(message),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn close(server: &CiServer, terminal: u64) {
        assert_eq!(
            server.answer(CiRequest::CloseTranscript { terminal }),
            CiReply::Closed
        );
    }

    fn alive(server: &CiServer, terminal: u64) -> bool {
        server.mux.session(TerminalId(terminal)).is_some()
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
        let mut failed = job("test", JobStatus::Failed, Some("mac"));
        failed.def.needs = vec!["build".to_owned()];
        failed.code = Some(3);
        failed.started = Some(10);
        failed.finished = Some(25);
        let record = pipeline(
            "view",
            3,
            "",
            PipelineStatus::Failed,
            vec![job("build", JobStatus::Succeeded, None), failed],
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
                ("test", "machine:mac", "failed")
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
    fn the_overview_lists_channels_and_what_runs_where() {
        let state = scratch("overview");
        store(
            &state,
            &pipeline(
                "view",
                1,
                "dev",
                PipelineStatus::Succeeded,
                vec![job("build", JobStatus::Succeeded, None)],
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
                    job("build", JobStatus::Running, None),
                    job("mac", JobStatus::Starting, Some("mac")),
                    job("test", JobStatus::Pending, None),
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
                vec![job("build", JobStatus::Failed, None)],
            ),
        );
        store(
            &state,
            &pipeline("other", 1, "dev", PipelineStatus::Succeeded, Vec::new()),
        );
        let mac = Machine::new(
            "mac",
            &MachineConfig::UnixHost(UnixHostConfig {
                ssh_host: "mac".to_owned(),
                dir: "/ci".to_owned(),
                cpus: 4,
                wait_minutes: 1,
            }),
            &state,
        );
        let server = server(&state, BTreeMap::from([("mac".to_owned(), mac)]), None);

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
        let machines: Vec<_> = seen
            .machines
            .iter()
            .map(|m| {
                (
                    m.name.as_str(),
                    m.kind.as_str(),
                    m.status.as_str(),
                    m.jobs.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            machines,
            [
                ("workstation", "workstation", "free", vec!["view #2 build"]),
                ("mac", "unix-host", "unknown", vec!["view #2 mac"]),
            ]
        );

        // A mode someone set says so; the measurement alone does not.
        server.busy.set_mode(BusyMode::Busy).expect("set the mode");
        assert_eq!(overview(&server).machines[0].status, "busy (set)");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_name_that_is_no_repository_reads_nothing() {
        let state = scratch("names");
        store(
            &state,
            &pipeline("view", 1, "dev", PipelineStatus::Succeeded, Vec::new()),
        );
        let server = server(&state, BTreeMap::new(), None);
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
        assert_eq!(
            server.answer(CiRequest::OpenTranscript {
                repo: sneaky.to_owned(),
                number: 1,
                job: "j0".to_owned()
            }),
            error("no such job")
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_transcript_replays_the_kept_wal_and_follows_a_running_job() {
        let state = scratch("args");
        let run = |name: &str| {
            let dir = state.join("runs").join(name);
            std::fs::create_dir_all(&dir).expect("create the run directory");
            dir
        };
        let args = |record: &JobRecord| {
            let record_pipeline = pipeline("view", 1, "dev", PipelineStatus::Running, Vec::new());
            replay_args(&state, &record_pipeline, record)
        };
        let file = |dir: &Path, name: &str| dir.join(name).to_string_lossy().into_owned();

        // Kept: the store wins even when the run's directory is gone.
        let kept = transcript_dir(&state, "view", 1, "kept");
        std::fs::create_dir_all(&kept).expect("create the store");
        std::fs::write(kept.join("wal"), "").expect("write the kept WAL");
        let mut pruned = job("kept", JobStatus::Succeeded, None);
        pruned.run_dir = Some(state.join("runs").join("gone"));
        assert_eq!(
            args(&pruned).expect("a kept transcript"),
            ["ci", "replay", &file(&kept, "wal")]
        );

        // Running: the run's WAL, followed until the run records its exit.
        let running_dir = run("running");
        std::fs::write(running_dir.join("wal"), "").expect("write the WAL");
        let mut running = job("running", JobStatus::Running, None);
        running.run_dir = Some(running_dir.clone());
        assert_eq!(
            args(&running).expect("a live transcript"),
            [
                "ci".to_owned(),
                "replay".to_owned(),
                file(&running_dir, "wal"),
                "--until".to_owned(),
                file(&running_dir, "exit"),
            ]
        );
        running.status = JobStatus::Failed;
        assert_eq!(
            args(&running).expect("a finished transcript"),
            ["ci", "replay", &file(&running_dir, "wal")]
        );

        // A run from before the WAL has a plain log, which is not followed.
        let legacy_dir = run("legacy");
        std::fs::write(legacy_dir.join("log"), "old output").expect("write the log");
        let mut legacy = job("legacy", JobStatus::Running, None);
        legacy.run_dir = Some(legacy_dir.clone());
        assert_eq!(
            args(&legacy).expect("a legacy transcript"),
            ["ci", "replay", &file(&legacy_dir, "log"), "--raw"]
        );

        // Nothing was written: a job that never ran, or never got that far.
        let never = job("never", JobStatus::Pending, None);
        assert_eq!(args(&never), Err("never has no transcript".to_owned()));
        let mut empty = job("empty", JobStatus::Failed, None);
        empty.run_dir = Some(run("empty"));
        assert_eq!(args(&empty), Err("empty has no transcript".to_owned()));
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_transcript_that_cannot_open_says_why() {
        let state = scratch("refused");
        with_transcripts(&state, 1);
        store(
            &state,
            &pipeline(
                "view",
                2,
                "dev",
                PipelineStatus::Running,
                vec![job("waiting", JobStatus::Pending, None)],
            ),
        );
        let stub = stub(&state, "exec sleep 600");

        let without_exe = server(&state, BTreeMap::new(), None);
        assert_eq!(
            open(&without_exe, "j0"),
            Err("no runner executable to replay with".to_owned())
        );

        let server = server(&state, BTreeMap::new(), Some(stub));
        assert_eq!(open(&server, "nope"), Err("no such job".to_owned()));
        let waiting = server.answer(CiRequest::OpenTranscript {
            repo: "view".to_owned(),
            number: 2,
            job: "waiting".to_owned(),
        });
        assert_eq!(waiting, error("waiting has no transcript"));
        let missing = server.answer(CiRequest::OpenTranscript {
            repo: "view".to_owned(),
            number: 9,
            job: "j0".to_owned(),
        });
        assert_eq!(missing, error("no such job"));
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn transcripts_are_reused_and_the_one_used_longest_ago_makes_room() {
        let state = scratch("lru");
        with_transcripts(&state, MAX_TRANSCRIPTS + 1);
        let server = server(
            &state,
            BTreeMap::new(),
            Some(stub(&state, "exec sleep 600")),
        );

        let first = open(&server, "j0").expect("open the first transcript");
        assert_eq!(
            open(&server, "j0"),
            Ok(first),
            "the same job is the same terminal"
        );
        let mut terminals = vec![first];
        for i in 1..MAX_TRANSCRIPTS {
            terminals.push(open(&server, &format!("j{i}")).expect("open a transcript"));
        }
        assert!(terminals.iter().all(|t| alive(&server, *t)));

        // A seventeenth closes the one used longest ago, which is j0: it was
        // reused before the others were opened.
        let last = open(&server, &format!("j{MAX_TRANSCRIPTS}")).expect("open one more");
        assert!(!alive(&server, first), "the least recently used is closed");
        assert!(alive(&server, terminals[1]));
        assert!(alive(&server, last));

        // Using j1 makes j2 the oldest; opening j0 again takes j2's place.
        assert_eq!(open(&server, "j1"), Ok(terminals[1]));
        let again = open(&server, "j0").expect("open the first one again");
        assert_ne!(again, first, "a closed transcript is a new terminal");
        assert!(!alive(&server, terminals[2]));
        assert!(alive(&server, terminals[1]));

        // Closing is the editor's, and idempotent.
        close(&server, terminals[1]);
        assert!(!alive(&server, terminals[1]));
        close(&server, terminals[1]);
        close(&server, 123_456);
        for terminal in terminals.iter().skip(3).chain([&last, &again]) {
            close(&server, *terminal);
        }
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_replay_that_ended_is_replaced_by_a_new_one() {
        let state = scratch("ended");
        with_transcripts(&state, 1);
        let server = server(&state, BTreeMap::new(), Some(stub(&state, "exit 1")));

        let first = open(&server, "j0").expect("open the transcript");
        let deadline = Instant::now() + Duration::from_secs(10);
        while server
            .mux
            .session(TerminalId(first))
            .is_some_and(|s| s.exit().is_none())
        {
            assert!(Instant::now() < deadline, "the stub never ended");
            std::thread::sleep(Duration::from_millis(20));
        }

        let second = open(&server, "j0").expect("open the transcript again");
        assert_ne!(second, first);
        assert!(!alive(&server, first), "the ended one is closed");
        close(&server, second);
        let _ = std::fs::remove_dir_all(&state);
    }
}
