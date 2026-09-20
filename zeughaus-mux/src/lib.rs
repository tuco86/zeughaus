//! The terminal multiplexer's wire model, shared by the runner that owns the
//! terminals and every editor that views them -- the wasm editor included.
//!
//! Pure types and a bounded codec. Nothing in here opens a PTY, parses an
//! escape sequence, draws a glyph or touches a socket; those live in
//! `zeughaus-terminal` (runner), `zeughaus-runner/src/mux.rs` (service) and
//! `zeughaus/src/mux.rs` plus `zeughaus/src/terminal/` (editor), and they meet
//! only here. The crate therefore has no dependency on tokio, iced, weida or
//! WezTerm, and no type of theirs ever crosses this boundary: a
//! [`CellStyle`] is a wire-stable set of colours and flags, never a
//! `CellAttributes`; a [`KeyInput`] is a semantic key, never an escape
//! sequence.
//!
//! The wire is a sequence of frames on one QUIC exchange. Every frame is a
//! fixed [`FrameHeader`] (length, protocol version, numeric message kind,
//! flags, request id) followed by a `postcard` body whose size the header
//! declares and the kind bounds ([`Kind::max_body`]) -- a declared length over
//! the bound is refused before anything is allocated. Enum ordering is never
//! the discriminant on the wire: kinds are explicit numbers, and adding a
//! variant to [`Message`] means assigning it a number in [`Kind`].
//!
//! [`FrameHeader`]: codec::FrameHeader
//! [`Kind::max_body`]: codec::Kind::max_body
//! [`Message`]: message::Message
//! [`CellStyle`]: terminal::CellStyle
//! [`KeyInput`]: input::KeyInput

pub mod codec;
pub mod id;
pub mod input;
pub mod message;
pub mod terminal;
pub mod view;
pub mod workspace;

pub use codec::{CodecError, FrameHeader, Kind, MAJOR, MINOR};
pub use id::{
    ClientInstanceId, PaneId, RequestId, RunnerIncarnation, SplitId, TabId, TerminalId, WorkspaceId,
};
pub use input::{
    KeyInput, Modifiers, MouseButton, MouseInput, MouseKind, NamedKey, TerminalCommand,
};
pub use message::{
    Capability, ClientHello, Command, CommandOutcome, CommandReply, ControlAttach, ControlAttached,
    ErrorCode, Message, RowFetch, RowPage, ServerHello, TerminalAttach, TerminalAttached,
    WireError,
};
pub use terminal::{
    CellSpan, CellStyle, Controller, Cursor, CursorShape, Dimensions, ExitState, Modes, Palette,
    RowData, StableRange, StyleFlags, TerminalDelta, TerminalEvent, TerminalHead, Underline,
    WireColor,
};
pub use workspace::{Axis, PaneNode, SurfaceRef, TabSnapshot, TopologyCommand, WorkspaceSnapshot};
