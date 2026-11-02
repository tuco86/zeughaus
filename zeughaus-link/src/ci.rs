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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::selector::Selector;

/// The largest body of one plan or proposal, in bytes.
pub const MAX_DOC_BYTES: usize = 256 << 10;

/// The longest title of one plan or proposal, in bytes.
pub const MAX_DOC_TITLE_BYTES: usize = 200;

/// How many lines under its title a plan may name its proposals in.
const PROPOSALS_WITHIN: usize = 20;

/// The title of a plan file: its first `# ` heading, cut to
/// [`MAX_DOC_TITLE_BYTES`] on a character boundary.
pub fn plan_title(body: &str) -> String {
    let Some(title) = body
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(str::trim)
        .filter(|title| !title.is_empty())
    else {
        return "Untitled plan".to_owned();
    };
    let mut end = title.len().min(MAX_DOC_TITLE_BYTES);
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    title[..end].trim_end().to_owned()
}

/// The proposals a plan names in a `Proposals: #3, #5` line among its first
/// lines; none without such a line.
pub fn proposals_named(body: &str) -> Result<Vec<u64>, String> {
    let Some(line) = body
        .lines()
        .take(PROPOSALS_WITHIN)
        .find(|line| line.starts_with("Proposals:"))
    else {
        return Ok(Vec::new());
    };
    let malformed = || format!("malformed Proposals line: {line}");
    let list = line["Proposals:".len()..].trim();
    if list.is_empty() {
        return Err(malformed());
    }
    list.split(',')
        .map(|item| {
            let digits = item.trim().strip_prefix('#').ok_or_else(malformed)?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(malformed());
            }
            digits.parse::<u64>().map_err(|_| malformed())
        })
        .collect()
}

/// The machines a plan needs, from a `Runs on: os=linux; os=windows` line
/// among its first lines: one label selector per machine, the first hosting
/// the session. None without such a line, which is any runner.
pub fn runs_on_named(body: &str) -> Result<Vec<String>, String> {
    let Some(line) = body
        .lines()
        .take(PROPOSALS_WITHIN)
        .find(|line| line.starts_with("Runs on:"))
    else {
        return Ok(Vec::new());
    };
    let malformed = || format!("malformed Runs on line: {line}");
    let list = line["Runs on:".len()..].trim();
    if list.is_empty() {
        return Err(malformed());
    }
    let selectors: Vec<String> = list.split(';').map(|part| part.trim().to_owned()).collect();
    for selector in &selectors {
        if selector.is_empty() {
            return Err(malformed());
        }
        Selector::parse(selector).map_err(|_| malformed())?;
    }
    Ok(selectors)
}

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

/// The longest pattern one [`CiRequest::Log`] may search for, in bytes.
pub const MAX_LOG_PATTERN_BYTES: usize = 1024;

/// The most lines of context a [`CiRequest::Log`] search shows around each
/// match, on either side.
pub const MAX_LOG_CONTEXT: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CiRequest {
    /// Every channel with its newest pipeline, and the machines.
    Overview,
    /// A channel's pipelines, newest first; an empty `channel` is every
    /// channel. `before` keeps only numbers below it; `limit` is clamped to
    /// `1..=`[`MAX_PIPELINES_PER_REQUEST`].
    Pipelines {
        repo: String,
        #[serde(default)]
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
    /// first output (`[+     1.5s] text`). With `pattern` only the lines
    /// that match the regular expression, each with `context` lines (at
    /// most [`MAX_LOG_CONTEXT`]) before and after it, searched within
    /// `lines` when given; `tail` is unused then. Answers [`CiReply::Log`],
    /// or an error when the job has no output.
    Log {
        repo: String,
        number: u64,
        job: String,
        tail: u32,
        #[serde(default)]
        timestamps: bool,
        #[serde(default)]
        lines: Vec<LineSpan>,
        #[serde(default)]
        pattern: Option<String>,
        #[serde(default)]
        context: u32,
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
        /// Also check the repository's setup (hook deliveries, the bot's
        /// access, its secrets and grants), which costs a forge call.
        #[serde(default)]
        setup: bool,
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
        /// One label selector per machine the work needs, the first hosting
        /// the session (see [`crate::selector`]).
        #[serde(default)]
        runs_on: Vec<String>,
        /// An onboarding request: the repository it asks the CI to add.
        #[serde(default)]
        onboard: Option<OnboardSpec>,
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
    /// Sets the `Runs on` of an open proposal or of a saved or approved
    /// plan. Answers [`CiReply::Accepted`].
    DocRunsOn {
        repo: String,
        number: u64,
        runs_on: Vec<String>,
    },
    /// Stops or resumes dispatching `repo`'s approved plans. Answers
    /// [`CiReply::Accepted`].
    QueuePaused { repo: String, paused: bool },
    /// Opens a plan session of `repo` on a runner that hosts sessions: the
    /// named one, else one matching the proposal's first `Runs on`, else
    /// any. Answers [`CiReply::Session`].
    OpenSession {
        repo: String,
        #[serde(default)]
        proposal: Option<u64>,
        #[serde(default)]
        runner: Option<String>,
    },
    /// One session. Answers [`CiReply::Session`].
    Session { id: u64 },
    /// Ends a live session's terminal. Answers [`CiReply::Accepted`].
    CloseSession { id: u64 },
    /// Queues an event of `repo` on its default branch: a push, or with
    /// `nightly` the repository's nightly, which waits for any other
    /// nightly that runs. The commit is the branch's head when the
    /// scheduler takes the event; `actor` (`user@host`) is who asked.
    /// Answers [`CiReply::Accepted`].
    Queue {
        repo: String,
        #[serde(default)]
        nightly: bool,
        actor: String,
    },
    /// Every job state store of a repository with its versions. Answers
    /// [`CiReply::States`].
    States { repo: String },
    /// Deletes version `version` of a store, or every version without one.
    /// Answers [`CiReply::Accepted`].
    StateDelete {
        repo: String,
        name: String,
        #[serde(default)]
        version: Option<u64>,
    },
    /// Takes `subject` for `holder`, or renews the holder's own claim on it.
    /// Answers [`CiReply::Claim`]; `ours` is false when someone else holds
    /// it, and the claim is theirs.
    Claim {
        subject: ClaimSubject,
        holder: ClaimHolder,
    },
    /// Renews every claim of `holder`. Answers [`CiReply::Accepted`].
    ClaimRenew { holder: ClaimHolder },
    /// Releases `subject`, or every claim of `holder` without one. Answers
    /// [`CiReply::Accepted`].
    ClaimRelease {
        holder: ClaimHolder,
        #[serde(default)]
        subject: Option<ClaimSubject>,
    },
    /// Who is responsible for pipeline `number` of `repo` (see
    /// [`responsible`]). Answers [`CiReply::Responsible`].
    Responsible { repo: String, number: u64 },
}

/// The one frame of a download on [`crate::STATE_PATH`]: version `version`
/// of the store `name` of `repo`. Sent as an enlist frame (postcard). The
/// reply is an [`crate::enlist::ArtifactReply`] frame; after `Sending`, the
/// encoded manifest as chunks and an end marker, then each distinct file's
/// blob in manifest order as a [`crate::enlist::BlobHead`] frame, its zstd
/// bytes as chunks and an end marker, and then the end of the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDownload {
    pub repo: String,
    pub name: String,
    pub version: u64,
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
        /// When no repository has the origin: the open or approved
        /// onboarding request whose url has it, without body.
        #[serde(default)]
        pending: Option<Box<DocView>>,
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
    States {
        states: Vec<StateView>,
    },
    Session {
        session: SessionView,
    },
    Claim {
        claim: ClaimView,
        ours: bool,
    },
    Responsible {
        claim: Option<ClaimView>,
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
    #[serde(default)]
    pub build: u64,
    /// The live agent sessions, then the newest ended ones.
    #[serde(default)]
    pub sessions: Vec<SessionView>,
    /// Every live claim.
    #[serde(default)]
    pub claims: Vec<ClaimView>,
}

/// What an agent session does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// omp in plan mode in the repository's plan clone.
    Plan,
    /// omp executing one approved plan in the master clone.
    Master,
}

impl SessionKind {
    pub fn name(self) -> &'static str {
        match self {
            SessionKind::Plan => "plan",
            SessionKind::Master => "master",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Sent to its runner, which has not answered yet.
    Starting,
    Running,
    Ended,
    /// It never ran: the runner refused it or went away.
    Failed,
}

impl SessionState {
    pub fn name(self) -> &'static str {
        match self {
            SessionState::Starting => "starting",
            SessionState::Running => "running",
            SessionState::Ended => "ended",
            SessionState::Failed => "failed",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, SessionState::Starting | SessionState::Running)
    }
}

/// One more machine a session works on, reached over ssh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMachine {
    pub name: String,
    #[serde(default)]
    pub ssh: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// An agent session the CI dispatched to a runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: u64,
    pub repo: String,
    pub kind: SessionKind,
    /// A master's plan, a plan session's proposal.
    #[serde(default)]
    pub doc: Option<u64>,
    /// The runner the session runs on.
    pub runner: String,
    #[serde(default)]
    pub machines: Vec<SessionMachine>,
    pub state: SessionState,
    /// Its terminal in the runner's mux, once it runs.
    #[serde(default)]
    pub terminal: Option<u64>,
    /// Unix seconds.
    pub started: u64,
    #[serde(default)]
    pub ended: Option<u64>,
    #[serde(default)]
    pub code: Option<i32>,
    /// Why it failed, or how it ended.
    #[serde(default)]
    pub message: String,
}

/// How long a claim lives without a renewal, in seconds.
pub const CLAIM_LEASE_SECS: u64 = 300;

/// What a claim is about: a proposal a session plans, or a pipeline a
/// session started or waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum ClaimSubject {
    Proposal { repo: String, number: u64 },
    Pipeline { repo: String, number: u64 },
}

impl ClaimSubject {
    pub fn repo(&self) -> &str {
        match self {
            ClaimSubject::Proposal { repo, .. } | ClaimSubject::Pipeline { repo, .. } => repo,
        }
    }

    pub fn number(&self) -> u64 {
        match self {
            ClaimSubject::Proposal { number, .. } | ClaimSubject::Pipeline { number, .. } => {
                *number
            }
        }
    }

    /// `proposal #3 of zeughaus`, `pipeline #12 of zeughaus`.
    pub fn text(&self) -> String {
        let kind = match self {
            ClaimSubject::Proposal { .. } => "proposal",
            ClaimSubject::Pipeline { .. } => "pipeline",
        };
        format!("{kind} #{} of {}", self.number(), self.repo())
    }
}

/// Who holds a claim: an agent's terminal on a runner, and the CI session
/// running in it, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimHolder {
    /// The runner's enlist name.
    pub runner: String,
    pub terminal: u64,
    #[serde(default)]
    pub session: Option<u64>,
}

impl ClaimHolder {
    /// The same terminal; the session is what the CI knows of it, not who
    /// it is.
    pub fn same(&self, other: &ClaimHolder) -> bool {
        self.runner == other.runner && self.terminal == other.terminal
    }
}

/// One claim the CI keeps: a holder is responsible for its subject until it
/// releases it, its session ends, or it is not renewed for
/// [`CLAIM_LEASE_SECS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimView {
    pub subject: ClaimSubject,
    pub holder: ClaimHolder,
    /// The holder's CI session as the editor names it (`master zeughaus
    /// #38`), or empty.
    #[serde(default)]
    pub label: String,
    /// A pipeline's channel; empty for a proposal.
    #[serde(default)]
    pub channel: String,
    /// Unix seconds.
    pub since: u64,
    pub renewed: u64,
}

impl ClaimView {
    /// `master zeughaus #38 on atik, terminal 7`, or `a session on atik,
    /// terminal 7` without a label.
    pub fn holder_text(&self) -> String {
        let who = if self.label.is_empty() {
            "a session"
        } else {
            &self.label
        };
        format!(
            "{who} on {}, terminal {}",
            self.holder.runner, self.holder.terminal
        )
    }
}

/// Who is responsible for pipeline `number` of `repo` in `channel`, whose
/// jobs `failed` failed: the holder of the earliest earlier pipeline of the
/// channel that failed one of the same jobs (`failed_of` names an earlier
/// pipeline's failed jobs, none when it is unknown), else the holder of
/// `number` itself.
pub fn responsible<'a>(
    claims: &'a [ClaimView],
    repo: &str,
    number: u64,
    channel: &str,
    failed: &[String],
    failed_of: impl Fn(u64) -> Vec<String>,
) -> Option<&'a ClaimView> {
    let own = || {
        claims.iter().find(|claim| {
            matches!(&claim.subject, ClaimSubject::Pipeline { repo: r, number: n }
                if r == repo && *n == number)
        })
    };
    if failed.is_empty() {
        return own();
    }
    let mut earlier: Vec<&ClaimView> = claims
        .iter()
        .filter(|claim| {
            matches!(&claim.subject, ClaimSubject::Pipeline { repo: r, number: n }
                if r == repo && *n < number)
                && claim.channel == channel
        })
        .collect();
    earlier.sort_by_key(|claim| claim.subject.number());
    earlier
        .into_iter()
        .find(|claim| {
            failed_of(claim.subject.number())
                .iter()
                .any(|job| failed.contains(job))
        })
        .or_else(own)
}

/// What a plan or a proposal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A change another project's session asks this repository for.
    Proposal,
    /// A finished plan of work in this repository.
    Plan,
    /// A project asks to join the CI as a new repository.
    Onboarding,
}

/// Where a document stands. A proposal is `open`, then `planned` or
/// `dismissed`; a `planned` proposal follows its plan, back to `open` when
/// the plan is rejected and to `done` when it is done. A plan is `saved`,
/// `approved`, `in_progress`, then `done`, or `rejected`. An onboarding
/// request is `open`, then `approved`, then `done` or back to `open` with a
/// note, or `rejected`.
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
    /// The wire name: `proposal`, `plan` or `onboarding`.
    pub fn name(self) -> &'static str {
        match self {
            DocKind::Proposal => "proposal",
            DocKind::Plan => "plan",
            DocKind::Onboarding => "onboarding",
        }
    }
}

/// The repository an onboarding request asks the CI to add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardSpec {
    /// The origin the request names; the CI stores its canonical clone url.
    pub url: String,
    #[serde(default = "main_branch")]
    pub default_branch: String,
    /// Secret name to the event patterns it is released to, as ci.toml's
    /// grants.
    #[serde(default)]
    pub grants: BTreeMap<String, Vec<String>>,
    /// Host prerequisites the owner confirms by hand, one line each.
    #[serde(default)]
    pub prerequisites: Vec<String>,
}

fn main_branch() -> String {
    "main".into()
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
    /// One label selector per machine the work needs, the first hosting
    /// the session; empty is any runner.
    #[serde(default)]
    pub runs_on: Vec<String>,
    /// A plan: the runners it runs or ran on, the host first.
    #[serde(default)]
    pub placement: Vec<String>,
    /// Empty in lists.
    #[serde(default)]
    pub body: String,
    /// An onboarding request: the repository it asks for.
    #[serde(default)]
    pub onboard: Option<OnboardSpec>,
    /// An onboarding request: why the CI's last attempt failed.
    #[serde(default)]
    pub note: String,
}

impl DocView {
    /// The text a reader is shown: the last failure of an onboarding
    /// request, the body, and what the request asks for.
    pub fn shown_text(&self) -> String {
        let mut text = String::new();
        if !self.note.is_empty() {
            text.push_str(&format!("> Last attempt failed: {}\n\n", self.note));
        }
        text.push_str(&self.body);
        if let Some(spec) = &self.onboard {
            let grants = if spec.grants.is_empty() {
                "none".to_owned()
            } else {
                spec.grants
                    .iter()
                    .map(|(name, patterns)| format!("{name}: {}", patterns.join(", ")))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            text.push_str(&format!(
                "\n\n## Onboarding\n\n- repository: {}\n- default branch: {}\n- grants: {grants}",
                spec.url, spec.default_branch
            ));
            if !spec.prerequisites.is_empty() {
                text.push_str("\n- host prerequisites, confirmed by the owner:");
                for line in &spec.prerequisites {
                    text.push_str(&format!("\n  - [ ] {line}"));
                }
            }
        }
        text
    }
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
    /// The plans in `in_progress`.
    #[serde(default)]
    pub in_progress: Vec<u64>,
    /// The CI dispatches none of its approved plans.
    #[serde(default)]
    pub paused: bool,
    /// Proposals in `open`.
    pub proposals: u32,
    /// The open or approved onboarding request's number while the
    /// repository is not in ci.toml yet.
    #[serde(default)]
    pub onboarding: Option<u64>,
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
            (DocKind::Onboarding, DocState::Open) => DocGroup::Awaiting,
            (DocKind::Onboarding, DocState::Approved) => DocGroup::InProgress,
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
    /// `push`, `tag`, `nightly`, `upstream` or `manual`.
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
    /// [`pipeline_load`] of the newest pipeline.
    #[serde(default)]
    pub load: Option<String>,
    /// For a channel whose newest pipeline failed: who is responsible for
    /// it (see [`responsible`]). Only [`RepoView`] fills it.
    #[serde(default)]
    pub responsible: Option<ClaimView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineView {
    pub repo: String,
    pub number: u64,
    pub channel: String,
    /// `push`, `tag`, `nightly`, `upstream` or `manual`.
    pub event: String,
    pub git_ref: String,
    pub sha: Option<String>,
    /// Who caused the pipeline: the pushing user, `nightly`, or whoever queued
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
    /// Expected run time in seconds, measured from the job's last succeeded
    /// runs at its place; `None` without one.
    #[serde(default)]
    pub expected_secs: Option<u64>,
    /// `<repo>#<n>/<job>` of every other job that was starting or running
    /// on the same runner while this one was; `None` in records from before
    /// it was kept. A VM job counts as on its runner.
    #[serde(default)]
    pub shared_with: Option<Vec<String>>,
}

impl JobView {
    /// Run time (`finished - started`) over `expected_secs`, in tenths
    /// rounded half up: 34 = 3.4x. `None` unless started, finished and
    /// `expected_secs` > 0.
    pub fn factor_tenths(&self) -> Option<u64> {
        let expected = self.expected_secs.filter(|&e| e > 0)?;
        let elapsed = self.finished?.saturating_sub(self.started?);
        Some((elapsed * 10 + expected / 2) / expected)
    }

    /// `3.4x expected` when the factor is known, then `shared its runner
    /// with N other jobs` (`1 other job`) or `runner not shared` when
    /// `shared_with` is known, joined by `, `. `None` when the job has not
    /// both started and finished, or neither part is known.
    pub fn load_text(&self) -> Option<String> {
        self.started?;
        self.finished?;
        let mut parts = Vec::new();
        if let Some(tenths) = self.factor_tenths() {
            parts.push(format!("{}.{}x expected", tenths / 10, tenths % 10));
        }
        match self.shared_with.as_ref().map(Vec::len) {
            Some(0) => parts.push("runner not shared".to_owned()),
            Some(1) => parts.push("shared its runner with 1 other job".to_owned()),
            Some(n) => parts.push(format!("shared its runner with {n} other jobs")),
            None => {}
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// `<job> <load_text>` of the job a finished pipeline is judged by: its
/// first `failed` job with a load text, else the job with the highest
/// factor (the first of equals; jobs without a factor rank last), else the
/// first job with a load text. `None` while `finished` is `None` or no job
/// has a load text.
pub fn pipeline_load(view: &PipelineView) -> Option<String> {
    view.finished?;
    let loaded: Vec<(&JobView, String)> = view
        .jobs
        .iter()
        .filter_map(|job| job.load_text().map(|text| (job, text)))
        .collect();
    let mut slowest: Option<(u64, &(&JobView, String))> = None;
    for entry in &loaded {
        if let Some(factor) = entry.0.factor_tenths()
            && slowest.is_none_or(|(best, _)| factor > best)
        {
            slowest = Some((factor, entry));
        }
    }
    let (job, text) = loaded
        .iter()
        .find(|(job, _)| job.status == "failed")
        .or(slowest.map(|(_, entry)| entry))
        .or(loaded.first())?;
    Some(format!("{} {text}", job.name))
}

/// One artifact set moving to the runner of a consuming job. Seconds since
/// the UNIX epoch; `finished` is `None` while it moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferView {
    pub need: String,
    pub started: u64,
    pub finished: Option<u64>,
    /// The sum of the set's file sizes.
    pub bytes: Option<u64>,
    /// The compressed blob bytes that crossed the wire for it.
    #[serde(default)]
    pub moved: Option<u64>,
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineView {
    pub name: String,
    /// `runner` (an enlisted runner, or one that is configured and offline)
    /// or `windows-vm` (a machine a runner reports).
    pub kind: String,
    pub status: String,
    /// The jobs running on it.
    pub jobs: Vec<MachineJob>,
    /// The build an enlisted runner runs; 0 when it is offline, for a
    /// machine, or unknown.
    #[serde(default)]
    pub build: u64,
    /// A runner's pinned fingerprint as `ci.toml` names it; empty for a
    /// machine.
    #[serde(default)]
    pub fingerprint: String,
    /// A runner's labels (`os=`, `arch=`, `runner=` and the configured
    /// ones), sorted.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The `ip:port` pairs an online runner's listener answers at, best
    /// first.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// The runner may host agent sessions.
    #[serde(default)]
    pub sessions: bool,
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
    /// With `setup`: one line per check of the repository's setup.
    #[serde(default)]
    pub setup: Vec<String>,
    /// This repository's live claims.
    #[serde(default)]
    pub claims: Vec<ClaimView>,
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
    /// The job state stores it keeps (`state = [..]`).
    #[serde(default)]
    pub state: Vec<String>,
    /// Whether a manual run (`run_jobs`) can run it: placed, and not on a
    /// machine.
    pub manual: bool,
}

/// One job state store of a repository with its versions, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateView {
    pub name: String,
    #[serde(default)]
    pub versions: Vec<StateVersionView>,
}

/// One version of a [`StateView`]: the pipeline that saved it, when (UNIX
/// seconds), and the sum and count of its files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateVersionView {
    pub number: u64,
    #[serde(default)]
    pub saved: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub files: u64,
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

    #[test]
    fn a_plan_is_titled_by_its_first_heading() {
        assert_eq!(
            plan_title("intro\n## Sub\n#  Real title  \n# Second"),
            "Real title"
        );
        assert_eq!(plan_title("no heading\n#nospace"), "Untitled plan");
        assert_eq!(plan_title("# \n"), "Untitled plan");
        // Cut on a character boundary: 'ä' is two bytes.
        let long = format!("# {}", "ä".repeat(150));
        let title = plan_title(&long);
        assert_eq!(title, "ä".repeat(100));
        assert!(title.len() <= MAX_DOC_TITLE_BYTES);
    }

    #[test]
    fn a_plan_names_its_proposals_in_one_line_near_the_top() {
        assert_eq!(
            proposals_named("# T\n\nProposals: #3, #5\n"),
            Ok(vec![3, 5])
        );
        assert_eq!(proposals_named("# T\nProposals:#7\n"), Ok(vec![7]));
        assert_eq!(proposals_named("# T\n\nbody\n"), Ok(Vec::new()));
        // Too far down is body text, not the line.
        let late = format!("# T\n{}Proposals: #1\n", "x\n".repeat(25));
        assert_eq!(proposals_named(&late), Ok(Vec::new()));
        for bad in [
            "Proposals: 3",
            "Proposals:",
            "Proposals: #3,",
            "Proposals: #3 #4",
            "Proposals: #x",
        ] {
            assert_eq!(
                proposals_named(&format!("# T\n{bad}\n")),
                Err(format!("malformed Proposals line: {bad}")),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_plan_names_its_machines_in_a_runs_on_line() {
        assert_eq!(
            runs_on_named("# T\nProposals: #3\nRuns on: os=linux ; os=windows,arch=amd64\n"),
            Ok(vec![
                "os=linux".to_owned(),
                "os=windows,arch=amd64".to_owned()
            ])
        );
        assert_eq!(runs_on_named("# T\n\nbody\n"), Ok(Vec::new()));
        for bad in ["Runs on:", "Runs on: os=linux;", "Runs on: OS=linux"] {
            assert_eq!(
                runs_on_named(&format!("# T\n{bad}\n")),
                Err(format!("malformed Runs on line: {bad}")),
                "{bad}"
            );
        }
    }

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
                    expected_secs: None,
                    shared_with: None,
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
                        moved: Some(2_000_000),
                        error: None,
                    }],
                    expected_secs: None,
                    shared_with: None,
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
            CiRequest::Queue {
                repo: "griasdi".to_owned(),
                nightly: true,
                actor: "hannes@tuco-pc".to_owned(),
            },
            CiRequest::Queue {
                repo: "griasdi".to_owned(),
                nightly: false,
                actor: "hannes@tuco-pc".to_owned(),
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
                pattern: None,
                context: 0,
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
                pattern: Some("panicked at|FAIL \\[".to_owned()),
                context: 2,
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
                setup: true,
            },
            CiRequest::Repo {
                origin: "/tmp/demo.git".to_owned(),
                sha: None,
                jobs: false,
                setup: false,
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
                runs_on: vec!["os=linux".to_owned(), "os=windows".to_owned()],
                onboard: None,
            },
            CiRequest::DocCreate {
                repo: "scan-rs".to_owned(),
                kind: DocKind::Onboarding,
                title: "Join".to_owned(),
                body: "Body".to_owned(),
                source: "scan-rs".to_owned(),
                proposals: Vec::new(),
                runs_on: Vec::new(),
                onboard: Some(spec()),
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
            CiRequest::DocRunsOn {
                repo: "griasdi".to_owned(),
                number: 3,
                runs_on: vec!["os=macos".to_owned()],
            },
            CiRequest::QueuePaused {
                repo: "griasdi".to_owned(),
                paused: true,
            },
            CiRequest::OpenSession {
                repo: "griasdi".to_owned(),
                proposal: Some(2),
                runner: None,
            },
            CiRequest::Session { id: 4 },
            CiRequest::CloseSession { id: 4 },
            CiRequest::Claim {
                subject: ClaimSubject::Proposal {
                    repo: "griasdi".to_owned(),
                    number: 3,
                },
                holder: ClaimHolder {
                    runner: "atik".to_owned(),
                    terminal: 7,
                    session: Some(4),
                },
            },
            CiRequest::ClaimRenew {
                holder: pipeline_claim("griasdi", "dev", 1, 7).holder,
            },
            CiRequest::ClaimRelease {
                holder: pipeline_claim("griasdi", "dev", 1, 7).holder,
                subject: Some(pipeline_claim("griasdi", "dev", 9, 7).subject),
            },
            CiRequest::ClaimRelease {
                holder: pipeline_claim("griasdi", "dev", 1, 7).holder,
                subject: None,
            },
            CiRequest::Responsible {
                repo: "griasdi".to_owned(),
                number: 9,
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
                        load: Some("build 3.4x expected, runner not shared".to_owned()),
                        responsible: None,
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
                            build: 116,
                            ..MachineView::default()
                        },
                        MachineView {
                            name: "win11".to_owned(),
                            kind: "windows-vm".to_owned(),
                            status: "off".to_owned(),
                            jobs: Vec::new(),
                            build: 0,
                            ..MachineView::default()
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
                        in_progress: vec![4, 6],
                        onboarding: Some(7),
                        paused: true,
                        proposals: 3,
                    }],
                    build: 116,
                    sessions: vec![session()],
                    claims: vec![pipeline_claim("griasdi", "dev", 7, 2)],
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
            CiReply::Repo {
                repo: None,
                pending: None,
            },
            CiReply::Repo {
                repo: None,
                pending: Some(Box::new(DocView {
                    kind: DocKind::Onboarding,
                    onboard: Some(spec()),
                    note: "collaborator: 403".to_owned(),
                    ..doc()
                })),
            },
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
                        load: None,
                        responsible: Some(pipeline_claim("zeughaus", "dev", 64, 2)),
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
                        state: vec!["corpus".to_owned()],
                        manual: true,
                    }],
                    jobs_note: String::new(),
                    machines: Vec::new(),
                    platforms: vec![("linux-amd64".to_owned(), "tuco-pc".to_owned())],
                    docs: vec![doc()],
                    setup: vec!["bot: push access to tuco86/zeughaus".to_owned()],
                    claims: vec![pipeline_claim("zeughaus", "dev", 64, 2)],
                })),
                pending: None,
            },
            CiReply::Docs { docs: vec![doc()] },
            CiReply::Doc {
                doc: Some(Box::new(doc())),
            },
            CiReply::Doc { doc: None },
            CiReply::DocCreated { number: 5 },
            CiReply::Session { session: session() },
            CiReply::Claim {
                claim: pipeline_claim("griasdi", "dev", 7, 2),
                ours: false,
            },
            CiReply::Responsible { claim: None },
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
                pattern,
                context,
                ..
            }) => assert_eq!(
                (tail, timestamps, lines, pattern, context),
                (200, false, Vec::new(), None, 0)
            ),
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
            runs_on: vec!["os=windows".to_owned()],
            placement: vec!["atik".to_owned(), "wohnzimmer".to_owned()],
            body: "Body".to_owned(),
            onboard: None,
            note: String::new(),
        }
    }

    fn spec() -> OnboardSpec {
        OnboardSpec {
            url: "https://git.doodleshnookie.net/tuco86/scan-rs.git".to_owned(),
            default_branch: "main".to_owned(),
            grants: BTreeMap::from([(
                "DEPLOY_KEY".to_owned(),
                vec!["push main".to_owned(), "tag v*".to_owned()],
            )]),
            prerequisites: vec!["a sudo rule for install.sh".to_owned()],
        }
    }

    #[test]
    fn an_onboarding_request_shows_its_failure_and_what_it_asks_for() {
        let request = DocView {
            kind: DocKind::Onboarding,
            body: "Body".to_owned(),
            onboard: Some(OnboardSpec {
                grants: BTreeMap::new(),
                ..spec()
            }),
            note: "collaborator: HTTP 403".to_owned(),
            ..doc()
        };
        assert_eq!(
            request.shown_text(),
            "> Last attempt failed: collaborator: HTTP 403\n\nBody\n\n## Onboarding\n\n\
             - repository: https://git.doodleshnookie.net/tuco86/scan-rs.git\n\
             - default branch: main\n- grants: none\n\
             - host prerequisites, confirmed by the owner:\n  - [ ] a sudo rule for install.sh"
        );
        assert_eq!(doc().shown_text(), "Body");
        let granted = DocView {
            onboard: Some(spec()),
            ..doc()
        };
        assert!(
            granted
                .shown_text()
                .contains("\n- grants: DEPLOY_KEY: push main, tag v*\n"),
            "{}",
            granted.shown_text()
        );
    }

    fn session() -> SessionView {
        SessionView {
            id: 3,
            repo: "griasdi".to_owned(),
            kind: SessionKind::Master,
            doc: Some(4),
            runner: "atik".to_owned(),
            machines: vec![SessionMachine {
                name: "wohnzimmer".to_owned(),
                ssh: Some("zeughaus@10.8.0.12".to_owned()),
                labels: vec!["os=windows".to_owned()],
                addresses: Vec::new(),
            }],
            state: SessionState::Running,
            terminal: Some(12),
            started: 100,
            ended: None,
            code: None,
            message: String::new(),
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
            runs_on: Vec::new(),
            onboard: None,
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

    /// A finished job that ran `secs` against `expected`, beside `shared`
    /// other jobs.
    fn ran(
        name: &str,
        status: &str,
        secs: u64,
        expected: Option<u64>,
        shared: Option<usize>,
    ) -> JobView {
        let mut job = pipeline().jobs[0].clone();
        job.name = name.to_owned();
        job.status = status.to_owned();
        job.started = Some(1000);
        job.finished = Some(1000 + secs);
        job.expected_secs = expected;
        job.shared_with = shared.map(|n| (0..n).map(|i| format!("r#{i}/j")).collect());
        job
    }

    #[test]
    fn a_jobs_factor_is_its_run_time_over_the_expected_in_tenths() {
        assert_eq!(
            ran("a", "succeeded", 204, Some(60), None).factor_tenths(),
            Some(34)
        );
        assert_eq!(
            ran("a", "succeeded", 29, Some(60), None).factor_tenths(),
            Some(5)
        );
        assert_eq!(
            ran("a", "succeeded", 29, Some(0), None).factor_tenths(),
            None
        );
        let mut running = ran("a", "running", 29, Some(60), None);
        running.finished = None;
        assert_eq!(running.factor_tenths(), None);
    }

    #[test]
    fn a_jobs_load_says_its_factor_and_who_shared_its_runner() {
        assert_eq!(
            ran("a", "succeeded", 204, Some(60), Some(2))
                .load_text()
                .as_deref(),
            Some("3.4x expected, shared its runner with 2 other jobs")
        );
        assert_eq!(
            ran("a", "succeeded", 60, Some(60), Some(0))
                .load_text()
                .as_deref(),
            Some("1.0x expected, runner not shared")
        );
        assert_eq!(
            ran("a", "succeeded", 9, None, Some(1))
                .load_text()
                .as_deref(),
            Some("shared its runner with 1 other job")
        );
        assert_eq!(ran("a", "succeeded", 60, None, None).load_text(), None);
        let mut running = ran("a", "running", 60, Some(60), Some(0));
        running.finished = None;
        assert_eq!(running.load_text(), None);
    }

    #[test]
    fn a_pipeline_is_judged_by_its_failed_job_else_its_slowest() {
        let mut view = pipeline();
        view.finished = Some(2000);
        view.jobs = vec![
            ran("fast", "succeeded", 60, Some(60), Some(0)),
            ran("slow", "succeeded", 300, Some(60), Some(1)),
            ran("broke", "failed", 90, Some(60), Some(0)),
        ];
        assert_eq!(
            pipeline_load(&view).as_deref(),
            Some("broke 1.5x expected, runner not shared")
        );
        view.jobs.pop();
        view.jobs
            .push(ran("unknown", "succeeded", 9, None, Some(3)));
        assert_eq!(
            pipeline_load(&view).as_deref(),
            Some("slow 5.0x expected, shared its runner with 1 other job")
        );
        view.jobs = vec![ran("unknown", "succeeded", 9, None, Some(3))];
        assert_eq!(
            pipeline_load(&view).as_deref(),
            Some("unknown shared its runner with 3 other jobs")
        );
        view.finished = None;
        assert_eq!(pipeline_load(&view), None);
    }

    fn pipeline_claim(repo: &str, channel: &str, number: u64, terminal: u64) -> ClaimView {
        ClaimView {
            subject: ClaimSubject::Pipeline {
                repo: repo.into(),
                number,
            },
            holder: ClaimHolder {
                runner: "atik".into(),
                terminal,
                session: None,
            },
            label: String::new(),
            channel: channel.into(),
            since: 0,
            renewed: 0,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_earliest_holder_of_the_same_failure_is_responsible() {
        let claims = vec![
            pipeline_claim("zeughaus", "dev", 12, 3),
            pipeline_claim("zeughaus", "dev", 10, 1),
            pipeline_claim("zeughaus", "dev", 11, 2),
            pipeline_claim("zeughaus", "dev", 14, 4),
        ];
        // #10 failed something else, #11 and #12 failed `check`.
        let failed_of = |number| match number {
            10 => names(&["build"]),
            11 | 12 => names(&["check", "lint"]),
            _ => Vec::new(),
        };
        let found = responsible(
            &claims,
            "zeughaus",
            14,
            "dev",
            &names(&["check"]),
            failed_of,
        );
        assert_eq!(found.map(|claim| claim.subject.number()), Some(11));
        // Nothing shared: the pipeline's own holder.
        let found = responsible(&claims, "zeughaus", 14, "dev", &names(&["fmt"]), failed_of);
        assert_eq!(found.map(|claim| claim.holder.terminal), Some(4));
        // Nothing failed: only the own claim counts.
        let found = responsible(&claims, "zeughaus", 14, "dev", &[], |_| names(&["check"]));
        assert_eq!(found.map(|claim| claim.holder.terminal), Some(4));
        // A pipeline nobody holds, without an earlier match.
        assert_eq!(
            responsible(&claims, "zeughaus", 15, "dev", &names(&["fmt"]), failed_of),
            None
        );
    }

    #[test]
    fn responsibility_stays_within_its_repository_and_channel() {
        let claims = vec![
            pipeline_claim("zeughaus", "nightly", 10, 1),
            pipeline_claim("weida", "dev", 11, 2),
        ];
        let found = responsible(&claims, "zeughaus", 14, "dev", &names(&["check"]), |_| {
            names(&["check"])
        });
        assert_eq!(found, None);
    }

    #[test]
    fn a_holder_is_its_terminal_whatever_session_the_ci_knows() {
        let holder = ClaimHolder {
            runner: "atik".into(),
            terminal: 7,
            session: Some(38),
        };
        let bare = ClaimHolder {
            session: None,
            ..holder.clone()
        };
        assert!(holder.same(&bare));
        let other = ClaimHolder {
            terminal: 8,
            ..holder.clone()
        };
        assert!(!holder.same(&other));
        let elsewhere = ClaimHolder {
            runner: "sadala".into(),
            ..holder.clone()
        };
        assert!(!holder.same(&elsewhere));
    }
}
