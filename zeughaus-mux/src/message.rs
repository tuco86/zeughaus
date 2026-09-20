//! The messages, and the exchanges they travel on.
//!
//! Three kinds of exchange, all on the one `/mux` path so weida reuses one
//! QUIC connection:
//!
//! - **Control**, one per client, long-lived. Opens with
//!   [`ControlAttach`] and is answered with [`ControlAttached`] -- hello,
//!   the whole topology and every terminal's head in one message, so the
//!   first paint needs one round trip. Then [`Command`]s go up and
//!   [`CommandReply`]s and fresh [`WorkspaceSnapshot`]s come down.
//! - **Terminal**, one per attached terminal, long-lived. Opens with
//!   [`TerminalAttach`], answered with [`TerminalAttached`]; then
//!   [`crate::TerminalCommand`]s go up and [`crate::TerminalDelta`]s (or a
//!   fresh [`crate::TerminalHead`] when a delta cannot be built) come down.
//!   The two directions are independent: a large delta never queues a
//!   keystroke behind it.
//! - **Row fetch**, short: [`RowFetch`] up, [`RowPage`] down, done. On its
//!   own stream so it cannot head-of-line block the other two.
//!
//! The first frame of an exchange names the exchange; a server that gets
//! anything else first answers with [`WireError`] and ends it.

use serde::{Deserialize, Serialize};

use crate::id::{ClientInstanceId, RequestId, RunnerIncarnation, TerminalId};
use crate::input::TerminalCommand;
use crate::terminal::{Dimensions, RowData, StableRange, TerminalDelta, TerminalHead};
use crate::workspace::{TopologyCommand, WorkspaceSnapshot};

/// A capability either side may offer. Unknown ones are ignored; known
/// ones are used only when both sides list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capability(pub u16);

impl Capability {
    /// Reserved for zstd on large snapshots and pages. Neither side offers
    /// it in this cut; the frame flag and the id are held so adding it is
    /// a minor version, not a redesign.
    pub const COMPRESSION: Capability = Capability(1);
}

/// What a client says first on the control exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub major: u8,
    pub minor: u8,
    pub client: ClientInstanceId,
    pub capabilities: Vec<Capability>,
    /// What the client still holds from a previous attach, so the runner can
    /// tell it whether any of it survived. Purely advisory: the runner
    /// always answers with the authoritative snapshot.
    pub known_incarnation: Option<RunnerIncarnation>,
    pub known_revision: Option<u64>,
}

/// What the runner answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub major: u8,
    /// The lower of the two minors: what both sides speak.
    pub minor: u8,
    pub incarnation: RunnerIncarnation,
    pub capabilities: Vec<Capability>,
    /// How the runner names this client to the others: the fingerprint it
    /// proved, or a certificate's name.
    pub principal: String,
    /// The profiles a terminal may be created from, by id and label.
    pub profiles: Vec<(u32, String)>,
}

/// Opens the control exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlAttach {
    pub hello: ClientHello,
}

/// Answers it: everything needed for a first paint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlAttached {
    pub hello: ServerHello,
    pub workspace: WorkspaceSnapshot,
    /// The head of every terminal in the workspace, with its visible rows.
    pub heads: Vec<TerminalHead>,
}

/// A topology command with its correlation id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub request: RequestId,
    pub command: TopologyCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandOutcome {
    /// Applied; the snapshot at `revision` follows or preceded this reply.
    Applied { revision: u64 },
    /// Not applied, and why, in words for the status bar.
    Refused { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub request: RequestId,
    pub outcome: CommandOutcome,
}

/// Opens a terminal exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalAttach {
    pub client: ClientInstanceId,
    pub terminal: TerminalId,
    /// The `(epoch, seq)` the client holds a head for, so the runner can
    /// continue with deltas instead of resending it.
    pub known: Option<(u64, u64)>,
    /// The size this client draws at. Applied as the terminal size only if
    /// this client controls it.
    pub size: Dimensions,
}

/// Answers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalAttached {
    /// A fresh head when the client's known state is not current; `None`
    /// when it is, and deltas continue from it.
    pub head: Option<TerminalHead>,
}

/// Asks for scrollback rows the client does not hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowFetch {
    pub terminal: TerminalId,
    pub epoch: u64,
    pub range: StableRange,
    /// Echoed in the page, so a reply to an older fetch cannot overwrite a
    /// newer row the client already has.
    pub generation: u64,
}

/// Most rows one fetch may ask for.
pub const MAX_FETCH_ROWS: u64 = 4096;

/// The rows of a fetch that still exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPage {
    pub terminal: TerminalId,
    pub epoch: u64,
    pub generation: u64,
    /// The oldest row retained at the time of the reply; rows of the fetch
    /// before it were evicted and are absent.
    pub first_retained: i64,
    /// The sequence number these rows are current at.
    pub seq: u64,
    pub rows: Vec<RowData>,
}

/// Why an exchange is being ended by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Protocol major mismatch.
    Version,
    /// The peer is not allowed to do this.
    Unauthorized,
    /// The first frame was not an attach, or a frame was malformed.
    Protocol,
    UnknownTerminal,
    /// The terminal exists but has no epoch/seq the request named.
    Stale,
    /// A bound was exceeded.
    Limit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

/// Every message that can be a frame body, with its numeric kind in
/// [`crate::Kind`].
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    ControlAttach(ControlAttach),
    ControlAttached(ControlAttached),
    Command(Command),
    CommandReply(CommandReply),
    WorkspaceSnapshot(WorkspaceSnapshot),
    TerminalAttach(TerminalAttach),
    TerminalAttached(TerminalAttached),
    TerminalDelta(TerminalDelta),
    TerminalHead(TerminalHead),
    TerminalCommand(TerminalCommand),
    RowFetch(RowFetch),
    RowPage(RowPage),
    Error(WireError),
}
