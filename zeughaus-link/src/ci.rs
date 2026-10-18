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
    /// than the CI's own cap. Answers [`CiReply::Log`], or an error when the
    /// job has no output.
    Log {
        repo: String,
        number: u64,
        job: String,
        tail: u32,
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
            CiRequest::Log {
                repo: "griasdi".to_owned(),
                number: 7,
                job: "build".to_owned(),
                tail: 200,
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
            CiReply::Error {
                message: "no such job".to_owned(),
            },
        ];
        for reply in replies {
            assert_eq!(CiReply::decode(&reply.encode()), Some(reply));
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
