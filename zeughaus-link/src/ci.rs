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

/// Largest request the runner reads: a few identifiers.
pub const MAX_CI_REQUEST_BYTES: usize = 4096;

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
    /// The tail of one job's stored output: at most `tail` lines, never more
    /// than the CI's own cap. With `timestamps` every line starts with the
    /// seconds since the job's first output (`[+     1.5s] text`) and a line
    /// with no text is left out. Answers [`CiReply::Log`], or an error when
    /// the job has no output.
    Log {
        repo: String,
        number: u64,
        job: String,
        tail: u32,
        #[serde(default)]
        timestamps: bool,
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
            },
            CiRequest::Log {
                repo: "griasdi".to_owned(),
                number: 7,
                job: "build".to_owned(),
                tail: 50,
                timestamps: true,
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
                })),
            },
            CiReply::Error {
                message: "no such job".to_owned(),
            },
        ];
        for reply in replies {
            assert_eq!(CiReply::decode(&reply.encode()), Some(reply));
        }
    }

    #[test]
    fn a_log_request_without_timestamps_still_decodes() {
        let old = br#"{"kind":"log","repo":"griasdi","number":7,"job":"build","tail":200}"#;
        match CiRequest::decode(old) {
            Some(CiRequest::Log {
                tail, timestamps, ..
            }) => assert_eq!((tail, timestamps), (200, false)),
            other => panic!("{other:?}"),
        }
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
}
