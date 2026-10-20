//! What an editor asks the CI about its pipelines, and what it gets back.
//!
//! One req/rep exchange on [`CI_PATH`](crate::CI_PATH): a [`CiRequest`] in,
//! a [`CiReply`] out, both JSON. The CI's pipeline records stay on the CI;
//! the views here carry only what the editor draws. A change of a
//! record is announced on `/events` as [`RuntimeEvent::CiPipeline`]
//! (crate::RuntimeEvent) and the editor then asks for the rows it shows.
//!
//! A transcript is a read-only terminal in the CI's mux:
//! [`CiRequest::OpenTranscript`] answers with the terminal id, which the
//! editor attaches through the normal mux exchanges.

use serde::{Deserialize, Serialize};

/// The largest body of one plan or proposal, in bytes.
pub const MAX_DOC_BYTES: usize = 256 << 10;

/// The longest title of one plan or proposal, in bytes.
pub const MAX_DOC_TITLE_BYTES: usize = 200;

/// Largest request the runner reads: a few identifiers, or one document
/// (JSON escaping can double a body).
pub const MAX_CI_REQUEST_BYTES: usize = MAX_DOC_BYTES * 2 + (16 << 10);

/// Largest reply an editor reads: up to 50 pipelines of a few dozen jobs.
pub const MAX_CI_REPLY_BYTES: usize = 4 << 20;

/// The most pipelines one [`CiRequest::Pipelines`] returns.
pub const MAX_PIPELINES_PER_REQUEST: u32 = 50;

/// The most lines one [`CiRequest::Timing`] returns.
pub const MAX_TIMING_LINES: u32 = 100;

/// How much of a job's output, counted from its end, the CI reads to answer
/// a [`CiRequest::Timing`]; [`JobTiming::truncated`] says when there was
/// more.
pub const TIMING_READ_BYTES: usize = 64 << 20;

/// The most spans one [`CiRequest::Log`] may name.
pub const MAX_LOG_SPANS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CiRequest {
    /// Every channel with its newest pipeline, and the machines.
    Overview,
    /// A channel's pipelines, newest first. `before` keeps only numbers below
    /// it; `limit` is clamped to `1..=`[`MAX_PIPELINES_PER_REQUEST`].
    Pipelines {
        repo: String,
        channel: String,
        before: Option<u64>,
        limit: u32,
    },
    /// One pipeline; `None` in the reply when it does not exist.
    Pipeline { repo: String, number: u64 },
    /// The newest pipeline of any channel (a try run included) whose commit
    /// id starts with `sha`, 4 to 40 hex digits; answers
    /// [`CiReply::Pipeline`], `None` when no pipeline matches. Only the
    /// newest 500 records of the repository are looked at.
    Find { repo: String, sha: String },
    /// A read-only terminal replaying one job's output, live while it runs.
    OpenTranscript {
        repo: String,
        number: u64,
        job: String,
    },
    /// Closes a terminal [`CiRequest::OpenTranscript`] returned.
    CloseTranscript { terminal: u64 },
    /// Queues a new pipeline for the event pipeline `number` ran for, pinned
    /// to the sha it recorded. Answers [`CiReply::Accepted`].
    Rerun { repo: String, number: u64 },
    /// Stops a running pipeline: its jobs are killed, the rest skipped.
    /// Answers [`CiReply::Accepted`], or an error when it is not running.
    Cancel { repo: String, number: u64 },
    /// One job's stored output, never more lines than the CI's own cap:
    /// the last `tail` lines, or with `lines` those lines instead. Every
    /// line starts with its number ([`LineSpan`] counts them). With
    /// `timestamps` the number is followed by the seconds since the job's
    /// first output (`[+     1.5s] text`). Answers [`CiReply::Log`], or an
    /// error when the job has no output.
    Log {
        repo: String,
        number: u64,
        job: String,
        tail: u32,
        #[serde(default)]
        timestamps: bool,
        #[serde(default)]
        lines: Vec<LineSpan>,
    },
    /// Where a job's time went: the `top` lines of its output (clamped to
    /// `1..=`[`MAX_TIMING_LINES`]) that the longest pauses follow, from the
    /// times the output arrived. Answers [`CiReply::Timing`], or an error
    /// when the job has no output.
    Timing {
        repo: String,
        number: u64,
        job: String,
        top: u32,
    },
    /// The repository of `ci.toml` whose `url` names the same remote as
    /// `origin` (a checkout's origin URL; at most [`MAX_ORIGIN_BYTES`]),
    /// for an agent in that checkout. `sha` (4 to 40 hex digits) names the
    /// checkout's HEAD; `jobs` asks for the jobs of its `.zeughaus-ci/`,
    /// which costs git reads on the CI's mirror. Answers [`CiReply::Repo`].
    Repo {
        origin: String,
        sha: Option<String>,
        jobs: bool,
    },
    /// Every plan and proposal of a repository, bodies empty, in number
    /// order. Answers [`CiReply::Docs`].
    Docs { repo: String },
    /// One document with its body; answers [`CiReply::Doc`], `None` when it
    /// does not exist.
    Doc { repo: String, number: u64 },
    /// Stores a new document. A plan may name the open proposals it was made
    /// from; they become `planned`. Answers [`CiReply::DocCreated`].
    DocCreate {
        repo: String,
        /// Renamed on the wire: `kind` is the request's own tag.
        #[serde(rename = "doc_kind")]
        kind: DocKind,
        title: String,
        body: String,
        source: String,
        #[serde(default)]
        proposals: Vec<u64>,
    },
    /// Moves a document to another state, where the state machine allows
    /// it. Answers [`CiReply::Accepted`].
    DocState {
        repo: String,
        number: u64,
        state: DocState,
    },
    /// Deletes a finished document. Answers [`CiReply::Accepted`].
    DocDelete { repo: String, number: u64 },
    /// The oldest approved plan becomes `in_progress` and comes back with its
    /// body; answers [`CiReply::Doc`], `None` when nothing is approved, an
    /// error while a plan is in progress.
    PlanClaim { repo: String },
}

/// Lines of a job's output, counted from 1 over the lines that have text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "span", rename_all = "snake_case")]
pub enum LineSpan {
    /// Lines `first..=last`; `last: None` runs to the end.
    Range { first: u32, last: Option<u32> },
    /// The last `count` lines.
    Last { count: u32 },
}

impl LineSpan {
    /// Parses the line selection `read` takes: comma-separated `N-M`, `N+K`
    /// (K lines from N), `N-` and `N` (to the end), `-K` (the last K). The
    /// error is meant to be shown as is.
    pub fn parse_list(text: &str) -> Result<Vec<LineSpan>, String> {
        if text.trim().is_empty() {
            return Err("lines: nothing selected".to_owned());
        }
        let parts: Vec<&str> = text.split(',').map(str::trim).collect();
        if parts.len() > MAX_LOG_SPANS {
            return Err(format!("lines: at most {MAX_LOG_SPANS} ranges"));
        }
        parts.into_iter().map(Self::parse_one).collect()
    }

    fn parse_one(part: &str) -> Result<LineSpan, String> {
        let unreadable =
            || format!("lines: cannot read \"{part}\"; use 120-180, 120+40, 120-, -60");
        let number = |text: &str| {
            let text = text.trim();
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(unreadable());
            }
            text.parse::<u32>().map_err(|_| unreadable())
        };
        let line = |text: &str| match number(text)? {
            0 => Err(format!("lines: line numbers count from 1 in \"{part}\"")),
            n => Ok(n),
        };
        let count = |text: &str| match number(text)? {
            0 => Err(unreadable()),
            n => Ok(n),
        };
        if let Some(rest) = part.strip_prefix('-') {
            return Ok(LineSpan::Last {
                count: count(rest)?,
            });
        }
        if let Some((first, more)) = part.split_once('+') {
            let first = line(first)?;
            let last = first.checked_add(count(more)? - 1).ok_or_else(unreadable)?;
            return Ok(LineSpan::Range {
                first,
                last: Some(last),
            });
        }
        if let Some((first, last)) = part.split_once('-') {
            let first = line(first)?;
            if last.trim().is_empty() {
                return Ok(LineSpan::Range { first, last: None });
            }
            let last = number(last)?;
            if last < first {
                return Err(format!("lines: {last} comes before {first} in \"{part}\""));
            }
            return Ok(LineSpan::Range {
                first,
                last: Some(last),
            });
        }
        Ok(LineSpan::Range {
            first: line(part)?,
            last: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CiReply {
    Overview {
        overview: CiOverview,
    },
    Pipelines {
        pipelines: Vec<PipelineView>,
    },
    /// Boxed: a pipeline is far larger than every other reply.
    Pipeline {
        pipeline: Option<Box<PipelineView>>,
    },
    /// The mux terminal id of the transcript.
    Transcript {
        terminal: u64,
    },
    Closed,
    /// A [`CiRequest::Rerun`] or [`CiRequest::Cancel`] was taken; the
    /// pipeline record changes later and is announced on `/events`.
    Accepted,
    /// The last lines of a job's output; `finished` is false while the job
    /// still runs and more may follow.
    Log {
        text: String,
        finished: bool,
    },
    /// The lines of a job's output that its longest pauses follow.
    Timing {
        timing: JobTiming,
    },
    /// `None` when no repository of `ci.toml` has the asked origin.
    Repo {
        repo: Option<Box<RepoView>>,
    },
    Docs {
        docs: Vec<DocView>,
    },
    /// Boxed: a document with its body is far larger than most replies.
    Doc {
        doc: Option<Box<DocView>>,
    },
    DocCreated {
        number: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CiOverview {
    pub channels: Vec<ChannelView>,
    pub machines: Vec<MachineView>,
    /// Events waiting for a pipeline, oldest first: behind a running
    /// pipeline of the same ref, or not yet picked up.
    #[serde(default)]
    pub queued: Vec<QueuedView>,
    /// The `[platforms]` of the CI's configuration: a platform name and the
    /// runner or machine that builds it, for a client that picks a target.
    #[serde(default)]
    pub platforms: Vec<(String, String)>,
    /// Every repository of `ci.toml` and its documents, in name order.
    #[serde(default)]
    pub repos: Vec<RepoDocsView>,
}

/// What a plan or a proposal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A change another project's session asks this repository for.
    Proposal,
    /// A finished plan of work in this repository.
    Plan,
}

/// Where a document stands. A proposal is `open`, then `planned` or
/// `dismissed`; a plan is `saved`, `approved`, `in_progress`, then `done`,
/// or `rejected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocState {
    Open,
    Planned,
    Dismissed,
    Saved,
    Approved,
    InProgress,
    Done,
    Rejected,
}

impl DocState {
    /// The wire name: `in_progress`, `open`, ...
    pub fn name(self) -> &'static str {
        match self {
            DocState::Open => "open",
            DocState::Planned => "planned",
            DocState::Dismissed => "dismissed",
            DocState::Saved => "saved",
            DocState::Approved => "approved",
            DocState::InProgress => "in_progress",
            DocState::Done => "done",
            DocState::Rejected => "rejected",
        }
    }
}

impl DocKind {
    /// The wire name: `proposal` or `plan`.
    pub fn name(self) -> &'static str {
        match self {
            DocKind::Proposal => "proposal",
            DocKind::Plan => "plan",
        }
    }
}

/// One plan or proposal of a repository. The CI stores exactly this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocView {
    pub repo: String,
    /// Per repository, shared by both kinds, never reused.
    pub number: u64,
    pub kind: DocKind,
    pub title: String,
    pub state: DocState,
    /// A proposal: the source project's checkout directory name; a plan: the
    /// repository itself.
    pub source: String,
    /// Seconds since the UNIX epoch.
    pub created: u64,
    /// The last state change.
    pub changed: u64,
    /// A plan: when it was approved; orders the queue.
    #[serde(default)]
    pub approved: Option<u64>,
    /// A plan: the proposals it was made from.
    #[serde(default)]
    pub proposals: Vec<u64>,
    /// A proposal: the plan that took it up.
    #[serde(default)]
    pub plan: Option<u64>,
    /// Empty in lists.
    #[serde(default)]
    pub body: String,
}

/// One repository of `ci.toml` and how many of its documents wait for whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDocsView {
    pub repo: String,
    /// The repository url as [`normalize_origin`] gives it: `host/owner/name`.
    pub origin: String,
    pub default_branch: String,
    /// Plans in `saved`.
    pub awaiting: u32,
    /// Plans in `approved`.
    pub queued: u32,
    /// The plan in `in_progress`.
    pub running: Option<u64>,
    /// Proposals in `open`.
    pub proposals: u32,
}

/// The groups a repository's documents are shown in, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocGroup {
    /// Plans in `saved`.
    Awaiting,
    /// Plans in `approved`, the next one first.
    Queue,
    /// The plan in `in_progress`.
    InProgress,
    /// Proposals in `open`.
    Proposals,
    /// Everything else, newest change first.
    History,
}

impl DocGroup {
    pub fn title(self) -> &'static str {
        match self {
            DocGroup::Awaiting => "Awaiting approval",
            DocGroup::Queue => "Queue",
            DocGroup::InProgress => "In progress",
            DocGroup::Proposals => "Open proposals",
            DocGroup::History => "History",
        }
    }

    fn of(doc: &DocView) -> DocGroup {
        match (doc.kind, doc.state) {
            (DocKind::Plan, DocState::Saved) => DocGroup::Awaiting,
            (DocKind::Plan, DocState::Approved) => DocGroup::Queue,
            (DocKind::Plan, DocState::InProgress) => DocGroup::InProgress,
            (DocKind::Proposal, DocState::Open) => DocGroup::Proposals,
            _ => DocGroup::History,
        }
    }
}

/// `docs` in their groups, the empty groups left out: the queue in the
/// order a master takes it, the history newest change first, the rest by
/// number.
pub fn doc_groups(docs: &[DocView]) -> Vec<(DocGroup, Vec<&DocView>)> {
    let mut groups = Vec::new();
    for group in [
        DocGroup::Awaiting,
        DocGroup::Queue,
        DocGroup::InProgress,
        DocGroup::Proposals,
        DocGroup::History,
    ] {
        let mut members: Vec<&DocView> = docs
            .iter()
            .filter(|doc| DocGroup::of(doc) == group)
            .collect();
        match group {
            DocGroup::Queue => members.sort_by_key(|doc| (doc.approved.unwrap_or(0), doc.number)),
            DocGroup::History => members.sort_by_key(|doc| {
                (
                    std::cmp::Reverse(doc.changed),
                    std::cmp::Reverse(doc.number),
                )
            }),
            _ => members.sort_by_key(|doc| doc.number),
        }
        if !members.is_empty() {
            groups.push((group, members));
        }
    }
    groups
}

/// A repository URL reduced to what names it: lowercase, without scheme,
/// user, `.git` and trailing slashes, an scp-style `host:path` as
/// `host/path`. `git@github.com:Griasdi/Griasdi.git` and
/// `https://github.com/griasdi/griasdi` come out the same.
pub fn normalize_origin(url: &str) -> String {
    let lower = url.trim().to_lowercase();
    let address = match lower.split_once("://") {
        Some((_scheme, rest)) => rest.to_owned(),
        // scp-style: `host:path`, where the host holds no `/` (a local path
        // that has a `:` further in is no host).
        None => match lower.split_once(':') {
            Some((host, path)) if !host.contains('/') => format!("{host}/{path}"),
            _ => lower,
        },
    };
    // The user (and password) belong to the authority, which ends at the
    // first `/`.
    let path_at = address.find('/').unwrap_or(address.len());
    let (authority, path) = address.split_at(path_at);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let joined = format!("{host}{path}");
    let trimmed = joined.trim_end_matches('/');
    trimmed
        .strip_suffix(".git")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_owned()
}

/// An event that has no pipeline yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedView {
    pub repo: String,
    /// The channel its pipeline is expected in: that of the newest pipeline
    /// of the same ref, else the one the event names without a
    /// `channels.toml`. The pipeline's own channel is decided when it starts.
    pub channel: String,
    /// `push`, `tag` or `cron`.
    pub event: String,
    pub git_ref: String,
    /// Pinned by a rerun or a webhook; `None` builds the ref's head.
    pub sha: Option<String>,
    pub actor: String,
    /// Seconds since the UNIX epoch.
    pub received: u64,
}

/// One channel of one repository and where its newest pipeline stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelView {
    pub repo: String,
    pub channel: String,
    /// The newest pipeline's number.
    pub latest: u64,
    /// That pipeline's status.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineView {
    pub repo: String,
    pub number: u64,
    pub channel: String,
    /// `push`, `tag` or `cron`.
    pub event: String,
    pub git_ref: String,
    pub sha: Option<String>,
    /// Who caused the pipeline: the pushing user, `cron`, or whoever queued
    /// it by hand. Empty from a runner that does not report it.
    #[serde(default)]
    pub actor: String,
    /// The first line of the commit's message; empty when it is not known.
    #[serde(default)]
    pub subject: String,
    pub status: String,
    pub note: String,
    /// Seconds since the UNIX epoch.
    pub created: u64,
    pub finished: Option<u64>,
    /// In the record's order.
    pub jobs: Vec<JobView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    pub name: String,
    pub needs: Vec<String>,
    /// Where the job runs: its runner, and the image or machine on it.
    pub place: String,
    pub status: String,
    pub note: String,
    pub started: Option<u64>,
    pub finished: Option<u64>,
    pub code: Option<i32>,
    /// The line that says why a failed job failed (`panicked at`, `error[`,
    /// `error:`, `FAILED`); `None` when it did not fail or nothing matches.
    #[serde(default)]
    pub cause: Option<String>,
    /// The stored excerpt of a failed job: the end of its output,
    /// escape-free.
    #[serde(default)]
    pub excerpt: Option<String>,
    /// When the job was handed to its runner; its staging runs from here to
    /// `started`. Seconds since the UNIX epoch.
    #[serde(default)]
    pub assigned: Option<u64>,
    /// The inputs that moved to the job's runner.
    #[serde(default)]
    pub transfers: Vec<TransferView>,
}

/// One artifact set moving to the runner of a consuming job. Seconds since
/// the UNIX epoch; `finished` is `None` while it moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferView {
    pub need: String,
    pub started: u64,
    pub finished: Option<u64>,
    pub bytes: Option<u64>,
    pub error: Option<String>,
}

/// How a job's output was spread over time, from the times its chunks
/// arrived on the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobTiming {
    /// The lines with text in the part of the output that was read.
    pub lines: u64,
    /// From the job's first output to its end, in milliseconds; a job that
    /// still runs ends now.
    pub span_ms: u64,
    /// The lines that the longest pauses follow, in output order.
    pub slowest: Vec<TimedLine>,
    /// The job ended: its exit record exists.
    pub finished: bool,
    /// Only the last [`TIMING_READ_BYTES`] of the output were read; the
    /// lines before them are missing.
    pub truncated: bool,
}

/// One line of a job's output and how long it stood last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedLine {
    /// Its number, counted from 1 as [`CiRequest::Log`] counts.
    #[serde(default)]
    pub line: u32,
    /// Milliseconds from the job's first output to this line's.
    pub at_ms: u64,
    /// Milliseconds from this line to the next one; for the last line, to
    /// the end of the job.
    pub took_ms: u64,
    /// As the transcript shows it, cut to a length a reply can carry.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineView {
    pub name: String,
    /// `runner` (an enlisted runner, or one that is configured and offline)
    /// or `windows-vm` (a machine a runner reports).
    pub kind: String,
    pub status: String,
    /// The jobs running on it.
    pub jobs: Vec<MachineJob>,
}

/// A job running on a machine, named so that an editor can open its
/// transcript in the channel's tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineJob {
    pub repo: String,
    pub channel: String,
    pub number: u64,
    pub job: String,
}

/// The longest origin a [`CiRequest::Repo`] may name, in bytes.
pub const MAX_ORIGIN_BYTES: usize = 1024;

/// What the CI knows of one of its repositories, for an agent in a checkout
/// of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoView {
    pub name: String,
    /// `"<forge> <slug>"`, or `"none"`.
    pub forge: String,
    pub default_branch: String,
    /// The default branch's state: `green`, or who is red since when.
    pub branch: String,
    /// This repository's channels, the newest pipeline of each.
    pub channels: Vec<ChannelView>,
    /// The newest pipeline of the asked sha, in any channel.
    pub head: Option<Box<PipelineView>>,
    /// The commit `jobs` were read from: the asked sha when the CI's mirror
    /// has it, else the head of the default branch.
    pub jobs_sha: Option<String>,
    pub jobs: Vec<JobDefView>,
    /// Why `jobs` is empty when it is; empty otherwise.
    pub jobs_note: String,
    pub machines: Vec<MachineView>,
    pub platforms: Vec<(String, String)>,
    /// The repository's documents in `open`, `saved`, `approved` and
    /// `in_progress`, bodies empty.
    #[serde(default)]
    pub docs: Vec<DocView>,
}

/// One job of a repository's `.zeughaus-ci/` and where it would run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobDefView {
    pub name: String,
    /// The job's file name in `.zeughaus-ci/`.
    pub file: String,
    pub on: Vec<String>,
    pub needs: Vec<String>,
    pub channels: Vec<String>,
    /// The header's placement: `host`, `image:<x>`, `platform:<p> image:<x>`, ...
    pub place: String,
    /// The runner the CI would place it on; empty when `placement` says why
    /// not.
    pub runner: String,
    /// Where on that runner: `host`, `image:<x>` or `machine:<vm>`.
    pub at: String,
    /// Why the job cannot be placed; empty when it can.
    pub placement: String,
    pub secrets: Vec<String>,
    /// `wait`, `freeze` or `run`.
    pub when_busy: String,
    /// Whether a manual run (`run_jobs`) can run it: placed, and not on a
    /// machine.
    pub manual: bool,
}

impl CiRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than [`MAX_CI_REQUEST_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_CI_REQUEST_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

impl CiReply {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// `None` for anything malformed or larger than [`MAX_CI_REPLY_BYTES`].
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_CI_REPLY_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline() -> PipelineView {
        PipelineView {
            repo: "griasdi".to_owned(),
            number: 7,
            channel: "dev".to_owned(),
            event: "push".to_owned(),
            git_ref: "main".to_owned(),
            sha: Some("0123456789abcdef".to_owned()),
            actor: "cebor".to_owned(),
            subject: "fix(ci): keep the header short".to_owned(),
            status: "running".to_owned(),
            note: String::new(),
            created: 1_700_000_000,
            finished: None,
            jobs: vec![
                JobView {
                    name: "build".to_owned(),
                    needs: Vec::new(),
                    place: "host".to_owned(),
                    status: "succeeded".to_owned(),
                    note: String::new(),
                    started: Some(1_700_000_001),
                    finished: Some(1_700_000_020),
                    code: Some(0),
                    cause: None,
                    excerpt: None,
                    assigned: Some(1_700_000_000),
                    transfers: Vec::new(),
                },
                JobView {
                    name: "test".to_owned(),
                    needs: vec!["build".to_owned()],
                    place: "win11".to_owned(),
                    status: "running".to_owned(),
                    note: "waiting".to_owned(),
                    started: None,
                    finished: None,
                    code: None,
                    cause: Some("error: boom".to_owned()),
                    excerpt: Some("error: boom".to_owned()),
                    assigned: Some(1_700_000_021),
                    transfers: vec![TransferView {
                        need: "build".to_owned(),
                        started: 1_700_000_021,
                        finished: Some(1_700_000_024),
                        bytes: Some(300_000_000),
                        error: None,
                    }],
                },
            ],
        }
    }

    #[test]
    fn every_request_round_trips() {
        let requests = [
            CiRequest::Overview,
            CiRequest::Pipelines {
                repo: "griasdi".to_owned(),
                channel: "dev".to_owned(),
                before: Some(40),
                limit: 20,
            },
            CiRequest::Pipelines {
                repo: "griasdi".to_owned(),
                channel: "release".to_owned(),
                before: None,
                limit: 1,
            },
            CiRequest::Pipeline {
                repo: "griasdi".to_owned(),
                number: 7,
            },
            CiRequest::OpenTranscript {
                repo: "griasdi".to_owned(),
                number: 7,
                job: "build".to_owned(),
            },
            CiRequest::CloseTranscript { terminal: 12 },
            CiRequest::Rerun {
                repo: "griasdi".to_owned(),
                number: 7,
            },
            CiRequest::Cancel {
                repo: "griasdi".to_owned(),
                number: 7,
            },
            CiRequest::Find {
                repo: "griasdi".to_owned(),
                sha: "0123abc".to_owned(),
            },
            CiRequest::Log {
                repo: "griasdi".to_owned(),
                number: 7,
                job: "build".to_owned(),
                tail: 200,
                timestamps: false,
                lines: Vec::new(),
            },
            CiRequest::Log {
                repo: "griasdi".to_owned(),
                number: 7,
                job: "build".to_owned(),
                tail: 50,
                timestamps: true,
                lines: vec![
                    LineSpan::Range {
                        first: 5,
                        last: Some(9),
                    },
                    LineSpan::Range {
                        first: 400,
                        last: None,
                    },
                    LineSpan::Last { count: 2 },
                ],
            },
            CiRequest::Timing {
                repo: "griasdi".to_owned(),
                number: 7,
                job: "build".to_owned(),
                top: 15,
            },
            CiRequest::Repo {
                origin: "ssh://git@git.doodleshnookie.net/tuco86/zeughaus.git".to_owned(),
                sha: Some("3635dad".to_owned()),
                jobs: true,
            },
            CiRequest::Repo {
                origin: "/tmp/demo.git".to_owned(),
                sha: None,
                jobs: false,
            },
            CiRequest::Docs {
                repo: "griasdi".to_owned(),
            },
            CiRequest::Doc {
                repo: "griasdi".to_owned(),
                number: 3,
            },
            CiRequest::DocCreate {
                repo: "griasdi".to_owned(),
                kind: DocKind::Plan,
                title: "Plan".to_owned(),
                body: "# Plan\n".to_owned(),
                source: "griasdi".to_owned(),
                proposals: vec![1, 2],
            },
            CiRequest::DocState {
                repo: "griasdi".to_owned(),
                number: 3,
                state: DocState::InProgress,
            },
            CiRequest::DocDelete {
                repo: "griasdi".to_owned(),
                number: 3,
            },
            CiRequest::PlanClaim {
                repo: "griasdi".to_owned(),
            },
        ];
        for request in requests {
            assert_eq!(CiRequest::decode(&request.encode()), Some(request));
        }
    }

    #[test]
    fn every_reply_round_trips() {
        let replies = [
            CiReply::Overview {
                overview: CiOverview {
                    channels: vec![ChannelView {
                        repo: "griasdi".to_owned(),
                        channel: "dev".to_owned(),
                        latest: 7,
                        status: "running".to_owned(),
                    }],
                    machines: vec![
                        MachineView {
                            name: "tuco-pc".to_owned(),
                            kind: "runner".to_owned(),
                            status: "free".to_owned(),
                            jobs: vec![MachineJob {
                                repo: "griasdi".to_owned(),
                                channel: "dev".to_owned(),
                                number: 7,
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
                        repo: "griasdi".to_owned(),
                        channel: "dev".to_owned(),
                        event: "push".to_owned(),
                        git_ref: "main".to_owned(),
                        sha: Some("abc".to_owned()),
                        actor: "rerun of #6".to_owned(),
                        received: 1,
                    }],
                    platforms: vec![("linux-amd64".to_owned(), "tuco-pc".to_owned())],
                    repos: vec![RepoDocsView {
                        repo: "griasdi".to_owned(),
                        origin: "github.com/griasdi/griasdi".to_owned(),
                        default_branch: "main".to_owned(),
                        awaiting: 1,
                        queued: 2,
                        running: Some(4),
                        proposals: 3,
                    }],
                },
            },
            CiReply::Pipelines {
                pipelines: vec![pipeline()],
            },
            CiReply::Pipeline {
                pipeline: Some(Box::new(pipeline())),
            },
            CiReply::Pipeline { pipeline: None },
            CiReply::Transcript { terminal: 9 },
            CiReply::Closed,
            CiReply::Accepted,
            CiReply::Log {
                text: "line\n".to_owned(),
                finished: true,
            },
            CiReply::Timing {
                timing: JobTiming {
                    lines: 3,
                    span_ms: 31_200,
                    slowest: vec![TimedLine {
                        line: 2,
                        at_ms: 0,
                        took_ms: 30_000,
                        text: "sleeping".to_owned(),
                    }],
                    finished: false,
                    truncated: true,
                },
            },
            CiReply::Repo { repo: None },
            CiReply::Repo {
                repo: Some(Box::new(RepoView {
                    name: "zeughaus".to_owned(),
                    forge: "forgejo tuco86/zeughaus".to_owned(),
                    default_branch: "main".to_owned(),
                    branch: "green".to_owned(),
                    channels: vec![ChannelView {
                        repo: "zeughaus".to_owned(),
                        channel: "dev".to_owned(),
                        latest: 64,
                        status: "succeeded".to_owned(),
                    }],
                    head: Some(Box::new(pipeline())),
                    jobs_sha: Some("3635dad0".to_owned()),
                    jobs: vec![JobDefView {
                        name: "publish".to_owned(),
                        file: "publish.sh".to_owned(),
                        on: Vec::new(),
                        needs: vec!["build-linux-amd64".to_owned()],
                        channels: vec!["dev".to_owned()],
                        place: "host".to_owned(),
                        runner: "sadala".to_owned(),
                        at: "host".to_owned(),
                        placement: String::new(),
                        secrets: vec!["ZEUGHAUS_UPDATE_KEY".to_owned()],
                        when_busy: "wait".to_owned(),
                        manual: true,
                    }],
                    jobs_note: String::new(),
                    machines: Vec::new(),
                    platforms: vec![("linux-amd64".to_owned(), "tuco-pc".to_owned())],
                    docs: vec![doc()],
                })),
            },
            CiReply::Docs { docs: vec![doc()] },
            CiReply::Doc {
                doc: Some(Box::new(doc())),
            },
            CiReply::Doc { doc: None },
            CiReply::DocCreated { number: 5 },
            CiReply::Error {
                message: "no such job".to_owned(),
            },
        ];
        for reply in replies {
            assert_eq!(CiReply::decode(&reply.encode()), Some(reply));
        }
    }

    #[test]
    fn a_log_request_without_timestamps_or_lines_still_decodes() {
        let old = br#"{"kind":"log","repo":"griasdi","number":7,"job":"build","tail":200}"#;
        match CiRequest::decode(old) {
            Some(CiRequest::Log {
                tail,
                timestamps,
                lines,
                ..
            }) => assert_eq!((tail, timestamps, lines), (200, false, Vec::new())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_timed_line_without_a_number_decodes_as_zero() {
        let old: TimedLine = serde_json::from_str(r#"{"at_ms":5,"took_ms":7,"text":"x"}"#).unwrap();
        assert_eq!(old.line, 0);
    }

    fn range(first: u32, last: Option<u32>) -> LineSpan {
        LineSpan::Range { first, last }
    }

    #[test]
    fn line_selections_parse_like_read() {
        assert_eq!(
            LineSpan::parse_list("120-180"),
            Ok(vec![range(120, Some(180))])
        );
        assert_eq!(
            LineSpan::parse_list("120+40"),
            Ok(vec![range(120, Some(159))])
        );
        assert_eq!(LineSpan::parse_list("7+1"), Ok(vec![range(7, Some(7))]));
        assert_eq!(LineSpan::parse_list("120-"), Ok(vec![range(120, None)]));
        assert_eq!(LineSpan::parse_list("120"), Ok(vec![range(120, None)]));
        assert_eq!(
            LineSpan::parse_list("-60"),
            Ok(vec![LineSpan::Last { count: 60 }])
        );
        assert_eq!(
            LineSpan::parse_list(" 1-20, 400+5 ,-3"),
            Ok(vec![
                range(1, Some(20)),
                range(400, Some(404)),
                LineSpan::Last { count: 3 }
            ])
        );
    }

    #[test]
    fn bad_line_selections_say_what_is_wrong() {
        assert_eq!(
            LineSpan::parse_list("  "),
            Err("lines: nothing selected".to_owned())
        );
        assert_eq!(
            LineSpan::parse_list("0-3"),
            Err("lines: line numbers count from 1 in \"0-3\"".to_owned())
        );
        assert_eq!(
            LineSpan::parse_list("1-5,9-4"),
            Err("lines: 4 comes before 9 in \"9-4\"".to_owned())
        );
        for bad in ["abc", "1-x", "-0", "5+0", "+5", "1-2-3", "1,,2"] {
            let part = if bad == "1,,2" { "" } else { bad };
            assert_eq!(
                LineSpan::parse_list(bad),
                Err(format!(
                    "lines: cannot read \"{part}\"; use 120-180, 120+40, 120-, -60"
                )),
                "{bad}"
            );
        }
        let many = vec!["1"; MAX_LOG_SPANS + 1].join(",");
        assert_eq!(
            LineSpan::parse_list(&many),
            Err("lines: at most 32 ranges".to_owned())
        );
    }

    #[test]
    fn garbage_and_oversized_bodies_decode_to_nothing() {
        assert!(CiRequest::decode(b"not json").is_none());
        assert!(CiRequest::decode(br#"{"kind":"nonsense"}"#).is_none());
        assert!(CiReply::decode(b"").is_none());
        assert!(CiReply::decode(br#"{"kind":"nonsense"}"#).is_none());

        let mut request = CiRequest::Overview.encode();
        request.resize(MAX_CI_REQUEST_BYTES + 1, b' ');
        assert!(CiRequest::decode(&request).is_none());
        request.truncate(MAX_CI_REQUEST_BYTES);
        assert_eq!(CiRequest::decode(&request), Some(CiRequest::Overview));

        let mut reply = CiReply::Closed.encode();
        reply.resize(MAX_CI_REPLY_BYTES + 1, b' ');
        assert!(CiReply::decode(&reply).is_none());
    }

    #[test]
    fn an_overview_without_platforms_still_decodes() {
        let old = br#"{"kind":"overview","overview":{"channels":[],"machines":[]}}"#;
        match CiReply::decode(old) {
            Some(CiReply::Overview { overview }) => assert!(overview.platforms.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    fn doc() -> DocView {
        DocView {
            repo: "griasdi".to_owned(),
            number: 3,
            kind: DocKind::Proposal,
            title: "Expose the clock".to_owned(),
            state: DocState::Planned,
            source: "weida".to_owned(),
            created: 10,
            changed: 20,
            approved: None,
            proposals: Vec::new(),
            plan: Some(4),
            body: "Body".to_owned(),
        }
    }

    #[test]
    fn a_doc_create_carries_its_kind_beside_the_request_tag() {
        let wire = CiRequest::DocCreate {
            repo: "r".to_owned(),
            kind: DocKind::Proposal,
            title: "t".to_owned(),
            body: "b".to_owned(),
            source: "s".to_owned(),
            proposals: Vec::new(),
        }
        .encode();
        let value: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(value["kind"], "doc_create");
        assert_eq!(value["doc_kind"], "proposal");
    }

    #[test]
    fn one_repository_is_named_the_same_by_every_url_form() {
        let same = [
            "git@github.com:griasdi/griasdi.git",
            "https://github.com/Griasdi/Griasdi.git",
            "https://github.com/griasdi/griasdi",
            "ssh://git@github.com/Griasdi/griasdi.git/",
            "https://user:token@github.com/griasdi/griasdi.git",
            " HTTP://GitHub.com/griasdi/griasdi.git\n",
        ];
        for url in same {
            assert_eq!(normalize_origin(url), "github.com/griasdi/griasdi", "{url}");
        }
        // Another repository, another host or another port is another name.
        assert_ne!(
            normalize_origin("git@github.com:griasdi/weida.git"),
            normalize_origin("git@github.com:griasdi/griasdi.git")
        );
        assert_eq!(
            normalize_origin("ssh://git@git.example.org:2222/o/r.git"),
            "git.example.org:2222/o/r"
        );
        // A local checkout is its path, with or without the scheme.
        assert_eq!(normalize_origin("/tmp/zh-fx/demo.git"), "/tmp/zh-fx/demo");
        assert_eq!(
            normalize_origin("file:///tmp/zh-fx/demo"),
            "/tmp/zh-fx/demo"
        );
    }

    #[test]
    fn documents_group_in_queue_order_with_the_newest_history_first() {
        let make = |number, kind, state, approved: Option<u64>, changed| DocView {
            number,
            kind,
            state,
            approved,
            changed,
            ..doc()
        };
        let docs = vec![
            make(1, DocKind::Plan, DocState::Done, None, 5),
            make(2, DocKind::Plan, DocState::Approved, Some(30), 30),
            make(3, DocKind::Proposal, DocState::Open, None, 1),
            make(4, DocKind::Plan, DocState::Approved, Some(20), 20),
            make(5, DocKind::Proposal, DocState::Dismissed, None, 9),
            make(6, DocKind::Plan, DocState::Saved, None, 2),
        ];
        let groups: Vec<(DocGroup, Vec<u64>)> = doc_groups(&docs)
            .into_iter()
            .map(|(group, docs)| (group, docs.iter().map(|doc| doc.number).collect()))
            .collect();
        assert_eq!(
            groups,
            vec![
                (DocGroup::Awaiting, vec![6]),
                (DocGroup::Queue, vec![4, 2]),
                (DocGroup::Proposals, vec![3]),
                (DocGroup::History, vec![5, 1]),
            ]
        );
    }
}
