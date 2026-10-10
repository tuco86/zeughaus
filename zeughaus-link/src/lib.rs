//! The link between a runner and its editors: everything that travels between
//! them over weida, and the identities both ends present.
//!
//! Each runner holds its own graph document and serves it on [`GRAPH_PATH`];
//! editors edit it through that exchange. Nothing a pass produces travels
//! there: a 3840x2160 RGBA frame is 33 MB, and a value that changes at frame
//! rate is not part of a document. Both travel over the same QUIC connection
//! on paths of their own, and this crate is what both ends speak -- pure data
//! and pure pixel math, no I/O, so the runtime's server and the editor's
//! client cannot disagree about the protocol.
//!
//! One listener, nine paths, each a different weida pattern:
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
//! | [`GRAPH_PATH`] | one long exchange per editor | [`graph`]: the document, edits and changes |
//!
//! [`credentials`] is the part both ends must agree on before any of that: the
//! files that hold the runner's identity and the client keys it trusts, and
//! the endpoint file through which a local editor finds its runner.

pub mod credentials;
pub mod events;
pub mod feed;
pub mod graph;
pub mod machine;
pub mod runs;

pub use events::{
    ErrorRow, MAX_EVENT_BYTES, MAX_SNAPSHOT_BYTES, MAX_TRIGGER_BYTES, OutputRow, RejectionRow,
    RuntimeEvent, Snapshot, TOPIC_CI, TOPIC_EDGE, TOPIC_ERROR, TOPIC_MACHINE, TOPIC_OUTPUT,
    TriggerRequest,
};
pub use feed::{
    FeedRequest, FrameHeader, MAX_DIMENSION, MAX_SAMPLES_PER_AXIS, ladder, scale_to_fit,
};
pub use graph::{GRAPH_MAJOR, GraphChange, GraphEdit, GraphMessage, MAX_GRAPH_FRAME_BYTES};
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

/// The graph document exchange: an editor attaches, receives the runner's
/// whole document and then its changes, and sends edits on the same exchange.
pub const GRAPH_PATH: &str = "/graph";
