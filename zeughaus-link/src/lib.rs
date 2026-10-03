//! The link between a runner and its editors: everything that travels between
//! them over weida, and the identities both ends present.
//!
//! Graph state travels through SpacetimeDB, but nothing a pass produces does: a
//! 3840x2160 RGBA frame is 33 MB, and a value that changes at frame rate is not
//! what a state store is for. Both travel over the runtime's own QUIC
//! connection instead, and this crate is what both ends speak -- pure data and
//! pure pixel math, no I/O, so the runtime's server and the editor's client
//! cannot disagree about the protocol.
//!
//! One listener, eight paths, each a different weida pattern:
//!
//! | path | pattern | carries |
//! |---|---|---|
//! | [`FEED_PATH`] | one exchange per (node, pin) | [`feed`]: scaled frames |
//! | [`EVENTS_PATH`] | pub/sub | [`events`]: outputs, node errors, edge traffic, the machine |
//! | [`SNAPSHOT_PATH`] | req/rep | [`events::Snapshot`] for a late joiner |
//! | [`TRIGGERS_PATH`] | push/pull | [`events::TriggerRequest`] from an editor |
//! | [`RUNS_PATH`] | req/rep | [`runs`]: a range of one file of one run |
//! | [`HOLD_PATH`] | req/rep | [`runs::HoldRequest`]: start no new runs |
//! | [`BUSY_PATH`] | req/rep | [`machine::BusyRequest`]: override the CI busy measurement |
//! | [`MUX_PATH`] | req/rep exchanges | the terminal mux (`zeughaus-mux`) |
//!
//! [`credentials`] is the part both ends must agree on before any of that: the
//! files that hold the runner's identity and the client keys it trusts.

pub mod credentials;
pub mod events;
pub mod feed;
pub mod machine;
pub mod runs;

pub use events::{
    ErrorRow, MAX_EVENT_BYTES, MAX_SNAPSHOT_BYTES, MAX_TRIGGER_BYTES, OutputRow, RejectionRow,
    RuntimeEvent, Snapshot, TOPIC_EDGE, TOPIC_ERROR, TOPIC_MACHINE, TOPIC_OUTPUT, TriggerRequest,
};
pub use feed::{
    FeedRequest, FrameHeader, MAX_DIMENSION, MAX_SAMPLES_PER_AXIS, ladder, scale_to_fit,
};
pub use machine::{BusyMode, BusyRequest, MAX_BUSY_BYTES, MachineState};
pub use runs::{
    HoldReply, HoldRequest, MAX_HOLD_BYTES, MAX_RUN_CHUNK_BYTES, MAX_RUN_REPLY_BYTES,
    MAX_RUN_REQUEST_BYTES, RunFileReply, RunFileRequest,
};

/// The endpoint a viewer dials for frames. Opaque to weida and matched
/// exactly, so it is the same string on both sides or nothing works.
pub const FEED_PATH: &str = "/samples";

/// Pub/Sub endpoint carrying runtime events (outputs and edge traffic).
pub const EVENTS_PATH: &str = "/events";

/// Req/Rep endpoint a late-joining editor asks for the current output set.
pub const SNAPSHOT_PATH: &str = "/snapshot";

/// Push/Pull endpoint an editor pushes manual trigger presses to.
pub const TRIGGERS_PATH: &str = "/triggers";

/// Req/Rep endpoint an editor fetches a run's files from: its log, its exit
/// record and its artifacts. On the runner that produced the run, because
/// that is the only process that holds the bytes.
pub const RUNS_PATH: &str = "/runs";

/// Req/Rep endpoint that holds and releases the runner: held, it starts no
/// new run and lets the live ones finish.
pub const HOLD_PATH: &str = "/hold";

/// Req/Rep endpoint that overrides a CI runner's busy measurement. Served
/// only by a runner that runs CI.
pub const BUSY_PATH: &str = "/busy";

/// Req/Rep endpoint carrying every terminal-mux exchange: the control
/// stream, one stream per attached terminal and the short scrollback
/// fetches. One path so weida pools them onto one QUIC connection -- the
/// pool key includes the path, so splitting them would cost a handshake
/// each and a warm attach would stop being warm.
pub const MUX_PATH: &str = "/mux";
