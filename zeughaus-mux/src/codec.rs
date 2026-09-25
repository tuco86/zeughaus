//! Frames: a fixed header, a bounded `postcard` body.
//!
//! ```text
//! 0      2     3     4      6      7        8         12                 20
//! +------+-----+-----+------+------+--------+---------+------------------+
//! | 'Z''M'| maj | min | kind | flags| reserv | length  | request id       |
//! +------+-----+-----+------+------+--------+---------+------------------+
//! ```
//!
//! All integers little-endian. `length` is the body's byte count and is
//! checked against the kind's bound before the body is read, so a hostile
//! peer cannot make either side allocate more than [`MAX_BODY`] bytes.
//! `request id` is meaningful for [`Kind::Command`] / [`Kind::CommandReply`]
//! and for [`Kind::RowFetch`] / [`Kind::RowPage`]; elsewhere it is zero.
//!
//! The body is `postcard`, which is compact, self-delimiting only through
//! `length`, and not self-describing: every enum here is decoded by its
//! declared variant order, which is why the variant order of every type in
//! this crate is part of the protocol and a new variant goes last.
//! [`Message::decode`] validates every bound the body declares before the
//! message is handed on -- a tree too deep, a row too wide, a title too
//! long -- so a decoded message is a valid one.

use std::fmt;

use serde::de::DeserializeOwned;

use crate::input::{MAX_TEXT_BYTES, TerminalCommand};
use crate::message::{ControlAttached, MAX_FETCH_ROWS, Message, RowFetch, RowPage};
use crate::terminal::{
    Dimensions, MAX_COLS, MAX_LINK_BYTES, MAX_ROWS, MAX_SPAN_BYTES, RowData, TerminalDelta,
    TerminalHead,
};
use crate::workspace::{
    MAX_TITLE_BYTES, MAX_TREE_DEPTH, MIN_RATIO, PaneNode, TopologyCommand, WorkspaceSnapshot,
};

/// Protocol version. A major mismatch refuses the attach; a minor is the
/// lower of the two sides'.
pub const MAJOR: u8 = 2;
pub const MINOR: u8 = 0;

const MAGIC: [u8; 2] = *b"ZM";

/// The largest body of any kind.
pub const MAX_BODY: u32 = 16 * 1024 * 1024;

/// Reserved: the body is zstd-compressed. Refused by both sides until the
/// capability is offered.
pub const FLAG_COMPRESSED: u8 = 1;

/// Most tabs in a snapshot (grouped ones included), and most leaves in a
/// tree.
pub const MAX_TABS: usize = 256;
pub const MAX_LEAVES: usize = 256;
/// Most groups in a snapshot.
pub const MAX_GROUPS: usize = 64;
/// Most owned terminals a snapshot may list as detached.
pub const MAX_DETACHED: usize = 256;
/// Most rows in one head, delta or page.
pub const MAX_ROWS_PER_MESSAGE: usize = 8192;
/// Most heads in one control attach.
pub const MAX_HEADS: usize = 256;
/// Most events in one delta.
pub const MAX_EVENTS: usize = 1024;

/// The numeric kind of a frame. Numbers are the wire; names may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Kind {
    ControlAttach = 0x01,
    ControlAttached = 0x02,
    Command = 0x03,
    CommandReply = 0x04,
    WorkspaceSnapshot = 0x05,
    TerminalAttach = 0x10,
    TerminalAttached = 0x11,
    TerminalDelta = 0x12,
    TerminalHead = 0x13,
    TerminalCommand = 0x14,
    RowFetch = 0x20,
    RowPage = 0x21,
    /// Reserved for image resources. No body type yet; a frame of this
    /// kind decodes to [`CodecError::Unsupported`].
    ResourceFetch = 0x30,
    /// Reserved, see [`Kind::ResourceFetch`].
    Resource = 0x31,
    Error = 0x7f,
}

impl Kind {
    pub fn from_u16(value: u16) -> Option<Kind> {
        Some(match value {
            0x01 => Kind::ControlAttach,
            0x02 => Kind::ControlAttached,
            0x03 => Kind::Command,
            0x04 => Kind::CommandReply,
            0x05 => Kind::WorkspaceSnapshot,
            0x10 => Kind::TerminalAttach,
            0x11 => Kind::TerminalAttached,
            0x12 => Kind::TerminalDelta,
            0x13 => Kind::TerminalHead,
            0x14 => Kind::TerminalCommand,
            0x20 => Kind::RowFetch,
            0x21 => Kind::RowPage,
            0x30 => Kind::ResourceFetch,
            0x31 => Kind::Resource,
            0x7f => Kind::Error,
            _ => return None,
        })
    }

    /// The largest body this kind may declare.
    pub fn max_body(self) -> u32 {
        const SMALL: u32 = 64 * 1024;
        const MEDIUM: u32 = 1024 * 1024;
        const LARGE: u32 = 8 * 1024 * 1024;
        match self {
            Kind::ControlAttach
            | Kind::Command
            | Kind::CommandReply
            | Kind::TerminalAttach
            | Kind::RowFetch
            | Kind::Error => SMALL,
            Kind::WorkspaceSnapshot => MEDIUM,
            // A paste of `MAX_TEXT_BYTES` plus the envelope.
            Kind::TerminalCommand => (MAX_TEXT_BYTES as u32) + SMALL,
            Kind::TerminalAttached | Kind::TerminalDelta | Kind::TerminalHead | Kind::RowPage => {
                LARGE
            }
            Kind::ControlAttached => MAX_BODY,
            Kind::ResourceFetch => SMALL,
            Kind::Resource => LARGE,
        }
    }
}

/// The fixed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub major: u8,
    pub minor: u8,
    pub kind: Kind,
    pub flags: u8,
    pub length: u32,
    pub request_id: u64,
}

impl FrameHeader {
    pub const LEN: usize = 20;

    pub fn new(kind: Kind, length: u32, request_id: u64) -> FrameHeader {
        FrameHeader {
            major: MAJOR,
            minor: MINOR,
            kind,
            flags: 0,
            length,
            request_id,
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[0..2].copy_from_slice(&MAGIC);
        out[2] = self.major;
        out[3] = self.minor;
        out[4..6].copy_from_slice(&(self.kind as u16).to_le_bytes());
        out[6] = self.flags;
        out[7] = 0;
        out[8..12].copy_from_slice(&self.length.to_le_bytes());
        out[12..20].copy_from_slice(&self.request_id.to_le_bytes());
        out
    }

    /// Parses and bounds a header. A header that passes is one whose body
    /// may be read: known kind, supported major, no unknown flag, and a
    /// length within the kind's bound.
    pub fn decode(bytes: &[u8; Self::LEN]) -> Result<FrameHeader, CodecError> {
        if bytes[0..2] != MAGIC {
            return Err(CodecError::Magic);
        }
        let major = bytes[2];
        if major != MAJOR {
            return Err(CodecError::Major(major));
        }
        let minor = bytes[3];
        let raw_kind = u16::from_le_bytes([bytes[4], bytes[5]]);
        let kind = Kind::from_u16(raw_kind).ok_or(CodecError::UnknownKind(raw_kind))?;
        let flags = bytes[6];
        if flags != 0 {
            return Err(CodecError::Flags(flags));
        }
        let length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if length > kind.max_body() {
            return Err(CodecError::TooLong {
                kind,
                length,
                max: kind.max_body(),
            });
        }
        let request_id = u64::from_le_bytes(bytes[12..20].try_into().expect("8 bytes"));
        Ok(FrameHeader {
            major,
            minor,
            kind,
            flags,
            length,
            request_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    Magic,
    Major(u8),
    UnknownKind(u16),
    Flags(u8),
    TooLong {
        kind: Kind,
        length: u32,
        max: u32,
    },
    /// A reserved kind with no body type.
    Unsupported(Kind),
    /// The body did not decode as the kind's type.
    Body(String),
    /// The body decoded but breaks a bound.
    Invalid(&'static str),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Magic => f.write_str("not a mux frame"),
            CodecError::Major(m) => write!(f, "protocol major {m}, this side speaks {MAJOR}"),
            CodecError::UnknownKind(k) => write!(f, "unknown frame kind {k:#x}"),
            CodecError::Flags(b) => write!(f, "unsupported frame flags {b:#x}"),
            CodecError::TooLong { kind, length, max } => {
                write!(f, "{kind:?} body of {length} bytes exceeds {max}")
            }
            CodecError::Unsupported(k) => write!(f, "{k:?} frames are reserved"),
            CodecError::Body(e) => write!(f, "malformed body: {e}"),
            CodecError::Invalid(what) => write!(f, "invalid body: {what}"),
        }
    }
}

impl std::error::Error for CodecError {}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::ControlAttach(_) => Kind::ControlAttach,
            Message::ControlAttached(_) => Kind::ControlAttached,
            Message::Command(_) => Kind::Command,
            Message::CommandReply(_) => Kind::CommandReply,
            Message::WorkspaceSnapshot(_) => Kind::WorkspaceSnapshot,
            Message::TerminalAttach(_) => Kind::TerminalAttach,
            Message::TerminalAttached(_) => Kind::TerminalAttached,
            Message::TerminalDelta(_) => Kind::TerminalDelta,
            Message::TerminalHead(_) => Kind::TerminalHead,
            Message::TerminalCommand(_) => Kind::TerminalCommand,
            Message::RowFetch(_) => Kind::RowFetch,
            Message::RowPage(_) => Kind::RowPage,
            Message::Error(_) => Kind::Error,
        }
    }

    /// The body alone. Validated first: a message this side would refuse to
    /// receive is refused here, where the bug is.
    pub fn encode_body(&self) -> Result<Vec<u8>, CodecError> {
        self.validate()?;
        let body = match self {
            Message::ControlAttach(m) => postcard::to_stdvec(m),
            Message::ControlAttached(m) => postcard::to_stdvec(m),
            Message::Command(m) => postcard::to_stdvec(m),
            Message::CommandReply(m) => postcard::to_stdvec(m),
            Message::WorkspaceSnapshot(m) => postcard::to_stdvec(m),
            Message::TerminalAttach(m) => postcard::to_stdvec(m),
            Message::TerminalAttached(m) => postcard::to_stdvec(m),
            Message::TerminalDelta(m) => postcard::to_stdvec(m),
            Message::TerminalHead(m) => postcard::to_stdvec(m),
            Message::TerminalCommand(m) => postcard::to_stdvec(m),
            Message::RowFetch(m) => postcard::to_stdvec(m),
            Message::RowPage(m) => postcard::to_stdvec(m),
            Message::Error(m) => postcard::to_stdvec(m),
        }
        .map_err(|e| CodecError::Body(e.to_string()))?;
        let max = self.kind().max_body();
        if body.len() > max as usize {
            return Err(CodecError::TooLong {
                kind: self.kind(),
                length: body.len() as u32,
                max,
            });
        }
        Ok(body)
    }

    /// Header plus body, ready to write.
    pub fn encode(&self, request_id: u64) -> Result<Vec<u8>, CodecError> {
        let body = self.encode_body()?;
        let header = FrameHeader::new(self.kind(), body.len() as u32, request_id);
        let mut out = Vec::with_capacity(FrameHeader::LEN + body.len());
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decodes and validates a body of `kind`.
    pub fn decode(kind: Kind, body: &[u8]) -> Result<Message, CodecError> {
        let message = match kind {
            Kind::ControlAttach => Message::ControlAttach(from(body)?),
            Kind::ControlAttached => Message::ControlAttached(from(body)?),
            Kind::Command => Message::Command(from(body)?),
            Kind::CommandReply => Message::CommandReply(from(body)?),
            Kind::WorkspaceSnapshot => Message::WorkspaceSnapshot(from(body)?),
            Kind::TerminalAttach => Message::TerminalAttach(from(body)?),
            Kind::TerminalAttached => Message::TerminalAttached(from(body)?),
            Kind::TerminalDelta => Message::TerminalDelta(from(body)?),
            Kind::TerminalHead => Message::TerminalHead(from(body)?),
            Kind::TerminalCommand => Message::TerminalCommand(from(body)?),
            Kind::RowFetch => Message::RowFetch(from(body)?),
            Kind::RowPage => Message::RowPage(from(body)?),
            Kind::Error => Message::Error(from(body)?),
            Kind::ResourceFetch | Kind::Resource => return Err(CodecError::Unsupported(kind)),
        };
        message.validate()?;
        Ok(message)
    }

    /// Every bound a body declares, checked. A message that passes can be
    /// handed to code that indexes by its numbers.
    pub fn validate(&self) -> Result<(), CodecError> {
        match self {
            Message::ControlAttach(m) => {
                if m.hello.capabilities.len() > 64 {
                    return Err(CodecError::Invalid("too many capabilities"));
                }
            }
            Message::ControlAttached(m) => {
                if m.hello.capabilities.len() > 64 {
                    return Err(CodecError::Invalid("too many capabilities"));
                }
                if m.hello.principal.len() > MAX_TITLE_BYTES {
                    return Err(CodecError::Invalid("principal too long"));
                }
                if m.hello.profiles.len() > 64
                    || m.hello
                        .profiles
                        .iter()
                        .any(|(_, l)| l.len() > MAX_TITLE_BYTES)
                {
                    return Err(CodecError::Invalid("profiles"));
                }
                validate_workspace(&m.workspace)?;
                if m.heads.len() > MAX_HEADS {
                    return Err(CodecError::Invalid("too many heads"));
                }
                for head in &m.heads {
                    validate_head(head)?;
                }
                validate_attached(m)?;
            }
            Message::Command(m) => validate_command(&m.command)?,
            Message::CommandReply(m) => {
                if let crate::message::CommandOutcome::Refused { reason } = &m.outcome
                    && reason.len() > MAX_TITLE_BYTES
                {
                    return Err(CodecError::Invalid("reason too long"));
                }
            }
            Message::WorkspaceSnapshot(m) => validate_workspace(m)?,
            Message::TerminalAttach(m) => validate_dimensions(m.size)?,
            Message::TerminalAttached(m) => {
                if let Some(head) = &m.head {
                    validate_head(head)?;
                }
            }
            Message::TerminalDelta(m) => validate_delta(m)?,
            Message::TerminalHead(m) => validate_head(m)?,
            Message::TerminalCommand(m) => validate_terminal_command(m)?,
            Message::RowFetch(m) => validate_fetch(m)?,
            Message::RowPage(m) => validate_page(m)?,
            Message::Error(m) => {
                if m.message.len() > MAX_TITLE_BYTES {
                    return Err(CodecError::Invalid("error message too long"));
                }
            }
        }
        Ok(())
    }
}

fn from<T: DeserializeOwned>(body: &[u8]) -> Result<T, CodecError> {
    postcard::from_bytes(body).map_err(|e| CodecError::Body(e.to_string()))
}

fn validate_attached(m: &ControlAttached) -> Result<(), CodecError> {
    // Every head names a terminal the topology shows, and each once.
    let mut seen = std::collections::HashSet::new();
    for head in &m.heads {
        if !seen.insert(head.terminal) {
            return Err(CodecError::Invalid("duplicate head"));
        }
    }
    Ok(())
}

fn validate_workspace(w: &WorkspaceSnapshot) -> Result<(), CodecError> {
    if w.tabs().count() > MAX_TABS {
        return Err(CodecError::Invalid("too many tabs"));
    }
    if w.groups().count() > MAX_GROUPS {
        return Err(CodecError::Invalid("too many groups"));
    }
    if w.groups().any(|g| g.name.len() > MAX_TITLE_BYTES) {
        return Err(CodecError::Invalid("group name too long"));
    }
    let mut leaves = 0;
    for tab in w.tabs() {
        if tab.title.len() > MAX_TITLE_BYTES {
            return Err(CodecError::Invalid("tab title too long"));
        }
        validate_tree(&tab.root, &mut leaves)?;
    }
    if leaves > MAX_LEAVES {
        return Err(CodecError::Invalid("too many panes"));
    }
    if w.detached.len() > MAX_DETACHED {
        return Err(CodecError::Invalid("too many detached terminals"));
    }
    for detached in &w.detached {
        if detached.title.len() > MAX_TITLE_BYTES {
            return Err(CodecError::Invalid("detached title too long"));
        }
    }
    Ok(())
}

fn validate_tree(node: &PaneNode, leaves: &mut usize) -> Result<(), CodecError> {
    if node.depth() > MAX_TREE_DEPTH {
        return Err(CodecError::Invalid("pane tree too deep"));
    }
    *leaves += node.leaf_count();
    validate_ratios(node)
}

fn validate_ratios(node: &PaneNode) -> Result<(), CodecError> {
    match node {
        PaneNode::Leaf { .. } => Ok(()),
        PaneNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !ratio.is_finite() || *ratio < MIN_RATIO || *ratio > 1.0 - MIN_RATIO {
                return Err(CodecError::Invalid("split ratio out of range"));
            }
            validate_ratios(first)?;
            validate_ratios(second)
        }
    }
}

fn validate_command(c: &TopologyCommand) -> Result<(), CodecError> {
    match c {
        TopologyCommand::ResizeSplit { ratio, .. }
            if !ratio.is_finite() || *ratio < MIN_RATIO || *ratio > 1.0 - MIN_RATIO =>
        {
            Err(CodecError::Invalid("split ratio out of range"))
        }
        TopologyCommand::RenameTab { title: Some(t), .. }
        | TopologyCommand::NewGroup { name: t, .. }
        | TopologyCommand::RenameGroup { name: t, .. }
            if t.len() > MAX_TITLE_BYTES =>
        {
            Err(CodecError::Invalid("title too long"))
        }
        _ => Ok(()),
    }
}

fn validate_dimensions(d: Dimensions) -> Result<(), CodecError> {
    if d.is_valid() {
        Ok(())
    } else {
        Err(CodecError::Invalid("dimensions out of range"))
    }
}

fn validate_row(row: &RowData) -> Result<(), CodecError> {
    if row.spans.len() > MAX_COLS as usize {
        return Err(CodecError::Invalid("too many spans in a row"));
    }
    let mut next_col = 0u32;
    for span in &row.spans {
        if span.text.len() > MAX_SPAN_BYTES {
            return Err(CodecError::Invalid("span text too long"));
        }
        if span.link.as_ref().is_some_and(|l| l.len() > MAX_LINK_BYTES) {
            return Err(CodecError::Invalid("link too long"));
        }
        if (span.start_col as u32) < next_col {
            return Err(CodecError::Invalid("spans overlap or run backwards"));
        }
        next_col = span.start_col as u32 + span.cell_count as u32;
        if next_col > MAX_COLS as u32 {
            return Err(CodecError::Invalid("span past the widest row"));
        }
    }
    Ok(())
}

fn validate_rows(rows: &[RowData]) -> Result<(), CodecError> {
    if rows.len() > MAX_ROWS_PER_MESSAGE {
        return Err(CodecError::Invalid("too many rows"));
    }
    rows.iter().try_for_each(validate_row)
}

fn validate_head(h: &TerminalHead) -> Result<(), CodecError> {
    validate_dimensions(h.dimensions)?;
    if h.visible.len() > MAX_ROWS as u64 {
        return Err(CodecError::Invalid("visible range too tall"));
    }
    if h.title.len() > MAX_TITLE_BYTES {
        return Err(CodecError::Invalid("title too long"));
    }
    if h.cursor.x >= h.dimensions.cols || h.cursor.y >= h.dimensions.rows {
        return Err(CodecError::Invalid("cursor outside the grid"));
    }
    validate_rows(&h.rows)
}

fn validate_delta(d: &TerminalDelta) -> Result<(), CodecError> {
    if d.to_seq < d.from_seq {
        return Err(CodecError::Invalid("delta runs backwards"));
    }
    if let Some(dim) = d.dimensions {
        validate_dimensions(dim)?;
    }
    if d.visible.is_some_and(|v| v.len() > MAX_ROWS as u64) {
        return Err(CodecError::Invalid("visible range too tall"));
    }
    if d.title.as_ref().is_some_and(|t| t.len() > MAX_TITLE_BYTES) {
        return Err(CodecError::Invalid("title too long"));
    }
    if d.ordered_events.len() > MAX_EVENTS {
        return Err(CodecError::Invalid("too many events"));
    }
    validate_rows(&d.row_replacements)
}

fn validate_terminal_command(c: &TerminalCommand) -> Result<(), CodecError> {
    match c {
        TerminalCommand::Text { text, .. } | TerminalCommand::Paste { text, .. } => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(CodecError::Invalid("text too long"));
            }
        }
        TerminalCommand::Resize(d) => validate_dimensions(*d)?,
        TerminalCommand::Viewport { rows, .. } => {
            if *rows == 0 || *rows > MAX_ROWS {
                return Err(CodecError::Invalid("viewport height out of range"));
            }
        }
        TerminalCommand::Mouse { input, .. } => {
            if input.col >= MAX_COLS || input.row >= MAX_ROWS {
                return Err(CodecError::Invalid("mouse outside the grid"));
            }
        }
        TerminalCommand::Key { .. } | TerminalCommand::Focus(_) => {}
    }
    Ok(())
}

fn validate_fetch(f: &RowFetch) -> Result<(), CodecError> {
    if f.range.is_empty() || f.range.len() > MAX_FETCH_ROWS {
        return Err(CodecError::Invalid("fetch range empty or too large"));
    }
    Ok(())
}

fn validate_page(p: &RowPage) -> Result<(), CodecError> {
    validate_rows(&p.rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::*;
    use crate::input::*;
    use crate::message::*;
    use crate::terminal::*;
    use crate::workspace::*;

    /// The header bytes are the protocol. If this changes, so has the wire,
    /// and MAJOR goes with it.
    #[test]
    fn header_golden_bytes() {
        let header = FrameHeader::new(Kind::TerminalDelta, 0x0002_0304, 0x1122_3344_5566_7788);
        let bytes = header.encode();
        assert_eq!(
            bytes,
            [
                b'Z', b'M', 2, 0, 0x12, 0x00, 0, 0, 0x04, 0x03, 0x02, 0x00, 0x88, 0x77, 0x66, 0x55,
                0x44, 0x33, 0x22, 0x11
            ]
        );
        assert_eq!(FrameHeader::decode(&bytes), Ok(header));
    }

    /// Kind numbers are the wire, not the enum order.
    #[test]
    fn kind_numbers_are_stable() {
        let expected = [
            (Kind::ControlAttach, 0x01),
            (Kind::ControlAttached, 0x02),
            (Kind::Command, 0x03),
            (Kind::CommandReply, 0x04),
            (Kind::WorkspaceSnapshot, 0x05),
            (Kind::TerminalAttach, 0x10),
            (Kind::TerminalAttached, 0x11),
            (Kind::TerminalDelta, 0x12),
            (Kind::TerminalHead, 0x13),
            (Kind::TerminalCommand, 0x14),
            (Kind::RowFetch, 0x20),
            (Kind::RowPage, 0x21),
            (Kind::ResourceFetch, 0x30),
            (Kind::Resource, 0x31),
            (Kind::Error, 0x7f),
        ];
        for (kind, number) in expected {
            assert_eq!(kind as u16, number);
            assert_eq!(Kind::from_u16(number), Some(kind));
        }
        assert_eq!(Kind::from_u16(0x06), None);
    }

    #[test]
    fn a_header_is_refused_before_its_body_is_read() {
        let mut bad = FrameHeader::new(Kind::Command, 0, 0).encode();
        bad[0] = b'X';
        assert_eq!(FrameHeader::decode(&bad), Err(CodecError::Magic));

        let mut major = FrameHeader::new(Kind::Command, 0, 0).encode();
        major[2] = MAJOR + 1;
        assert_eq!(
            FrameHeader::decode(&major),
            Err(CodecError::Major(MAJOR + 1))
        );

        let mut kind = FrameHeader::new(Kind::Command, 0, 0).encode();
        kind[4] = 0x77;
        assert_eq!(
            FrameHeader::decode(&kind),
            Err(CodecError::UnknownKind(0x77))
        );

        let mut flags = FrameHeader::new(Kind::Command, 0, 0).encode();
        flags[6] = FLAG_COMPRESSED;
        assert_eq!(FrameHeader::decode(&flags), Err(CodecError::Flags(1)));

        let long = FrameHeader::new(Kind::Command, Kind::Command.max_body() + 1, 0).encode();
        assert!(matches!(
            FrameHeader::decode(&long),
            Err(CodecError::TooLong { .. })
        ));
    }

    fn head() -> TerminalHead {
        TerminalHead {
            terminal: TerminalId(3),
            epoch: 1,
            seq: 40,
            dimensions: Dimensions { cols: 80, rows: 24 },
            visible: StableRange {
                start: 100,
                end: 124,
            },
            first_retained: 0,
            cursor: Cursor {
                x: 5,
                y: 2,
                shape: CursorShape::Block,
                visible: true,
                blinking: true,
            },
            title: "bash".into(),
            modes: Modes::default(),
            palette: Palette::default(),
            rows: vec![RowData {
                stable_row: 100,
                row_seq: 39,
                wrapped: false,
                spans: vec![CellSpan {
                    start_col: 0,
                    cell_count: 3,
                    text: "a\u{4e2d}".into(),
                    style: CellStyle {
                        fg: WireColor::Indexed(2),
                        bg: WireColor::Default,
                        underline_color: WireColor::Rgb([1, 2, 3]),
                        flags: StyleFlags::default().with(StyleFlags::BOLD),
                    },
                    link: None,
                }],
            }],
            exit: None,
            controller: Some(Controller {
                client: ClientInstanceId::from_bytes([9; 16]),
                principal: "sha256:ab".into(),
            }),
            input_serial_ack: 7,
        }
    }

    fn workspace() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            incarnation: RunnerIncarnation::from_bytes([1; 16]),
            revision: 4,
            items: vec![
                WorkspaceItem::Tab(TabSnapshot {
                    id: TabId(1),
                    title: "main".into(),
                    accent_rgba: Some([1, 2, 3, 255]),
                    root: PaneNode::Split {
                        id: SplitId(1),
                        axis: Axis::Horizontal,
                        ratio: 0.4,
                        first: Box::new(PaneNode::Leaf {
                            pane_id: PaneId(1),
                            surface: SurfaceRef::Graph(77),
                        }),
                        second: Box::new(PaneNode::Leaf {
                            pane_id: PaneId(2),
                            surface: SurfaceRef::Terminal(TerminalId(3)),
                        }),
                    },
                }),
                WorkspaceItem::Group(GroupSnapshot {
                    id: GroupId(5),
                    name: "Triggered".into(),
                    color_rgba: [128, 128, 128, 64],
                    locked: true,
                    tabs: vec![TabSnapshot {
                        id: TabId(2),
                        title: "run".into(),
                        accent_rgba: None,
                        root: PaneNode::Leaf {
                            pane_id: PaneId(3),
                            surface: SurfaceRef::Terminal(TerminalId(5)),
                        },
                    }],
                }),
            ],
            detached: vec![DetachedTerminal {
                terminal: TerminalId(4),
                title: "cargo test".into(),
            }],
        }
    }

    fn round_trip(message: Message) {
        let bytes = message.encode(42).expect("encode");
        let header = FrameHeader::decode(bytes[..FrameHeader::LEN].try_into().unwrap()).unwrap();
        assert_eq!(header.kind, message.kind());
        assert_eq!(header.request_id, 42);
        assert_eq!(header.length as usize, bytes.len() - FrameHeader::LEN);
        let decoded = Message::decode(header.kind, &bytes[FrameHeader::LEN..]).expect("decode");
        assert_eq!(decoded, message);
    }

    #[test]
    fn every_body_round_trips() {
        round_trip(Message::ControlAttach(ControlAttach {
            hello: ClientHello {
                major: MAJOR,
                minor: MINOR,
                client: ClientInstanceId::from_bytes([2; 16]),
                capabilities: vec![Capability::COMPRESSION],
                known_incarnation: None,
                known_revision: Some(3),
            },
        }));
        round_trip(Message::ControlAttached(ControlAttached {
            hello: ServerHello {
                major: MAJOR,
                minor: MINOR,
                incarnation: RunnerIncarnation::from_bytes([1; 16]),
                capabilities: vec![],
                principal: "sha256:ab".into(),
                profiles: vec![(0, "bash".into())],
            },
            workspace: workspace(),
            heads: vec![head()],
        }));
        round_trip(Message::Command(Command {
            request: RequestId(9),
            command: TopologyCommand::SplitWithTerminal {
                pane: PaneId(2),
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
        }));
        round_trip(Message::Command(Command {
            request: RequestId(10),
            command: TopologyCommand::AttachTerminal {
                terminal: TerminalId(4),
                target: AttachTarget::Split {
                    pane: PaneId(2),
                    axis: Axis::Horizontal,
                },
            },
        }));
        round_trip(Message::Command(Command {
            request: RequestId(11),
            command: TopologyCommand::AttachTerminal {
                terminal: TerminalId(4),
                target: AttachTarget::NewTab,
            },
        }));
        round_trip(Message::Command(Command {
            request: RequestId(12),
            command: TopologyCommand::CloseTerminal {
                terminal: TerminalId(4),
            },
        }));
        for command in [
            TopologyCommand::OpenGraph { graph: 9 },
            TopologyCommand::NewGroup {
                name: "Group 1".into(),
                color_rgba: [1, 2, 3, 4],
            },
            TopologyCommand::MoveTab {
                tab: TabId(1),
                to: TabSlot {
                    group: Some(GroupId(2)),
                    index: u32::MAX,
                },
            },
            TopologyCommand::MergeTab {
                tab: TabId(1),
                pane: PaneId(4),
                side: Side::Top,
            },
            TopologyCommand::MovePane {
                pane: PaneId(4),
                to: PaneTarget::Beside {
                    pane: PaneId(5),
                    side: Side::Right,
                },
            },
        ] {
            round_trip(Message::Command(Command {
                request: RequestId(13),
                command,
            }));
        }
        round_trip(Message::CommandReply(CommandReply {
            request: RequestId(9),
            outcome: CommandOutcome::Refused {
                reason: "the graph pane stays".into(),
            },
        }));
        round_trip(Message::WorkspaceSnapshot(workspace()));
        round_trip(Message::TerminalAttach(TerminalAttach {
            client: ClientInstanceId::from_bytes([2; 16]),
            terminal: TerminalId(3),
            known: Some((1, 40)),
            size: Dimensions {
                cols: 100,
                rows: 30,
            },
        }));
        round_trip(Message::TerminalAttached(TerminalAttached {
            head: Some(head()),
        }));
        round_trip(Message::TerminalHead(head()));
        round_trip(Message::TerminalDelta(TerminalDelta {
            terminal: TerminalId(3),
            epoch: 1,
            from_seq: 40,
            to_seq: 41,
            input_serial_ack: 8,
            dimensions: None,
            visible: Some(StableRange {
                start: 101,
                end: 125,
            }),
            cursor: None,
            title: Some("vim".into()),
            modes: Some(Modes {
                alt_screen: true,
                ..Modes::default()
            }),
            palette: None,
            evicted_before: Some(5),
            row_replacements: head().rows,
            ordered_events: vec![
                TerminalEvent::Bell,
                TerminalEvent::Exited(ExitState::Exited { code: 0 }),
            ],
        }));
        round_trip(Message::TerminalCommand(TerminalCommand::Key {
            serial: 1,
            input: KeyInput {
                key: crate::input::Key::Named(NamedKey::F(5)),
                modifiers: Modifiers::default().with(Modifiers::CTRL),
            },
        }));
        round_trip(Message::TerminalCommand(TerminalCommand::Mouse {
            serial: 2,
            input: MouseInput {
                kind: MouseKind::Press,
                button: Some(MouseButton::Left),
                col: 3,
                row: 4,
                modifiers: Modifiers::default(),
            },
        }));
        round_trip(Message::RowFetch(RowFetch {
            terminal: TerminalId(3),
            epoch: 1,
            range: StableRange { start: 0, end: 100 },
            generation: 2,
        }));
        round_trip(Message::RowPage(RowPage {
            terminal: TerminalId(3),
            epoch: 1,
            generation: 2,
            first_retained: 10,
            seq: 41,
            rows: head().rows,
        }));
        round_trip(Message::Error(WireError {
            code: ErrorCode::Unauthorized,
            message: "no".into(),
        }));
    }

    #[test]
    fn a_reserved_kind_has_no_body() {
        assert_eq!(
            Message::decode(Kind::Resource, &[]),
            Err(CodecError::Unsupported(Kind::Resource))
        );
    }

    #[test]
    fn bounds_are_checked_after_decoding() {
        let mut deep = workspace();
        let mut node = PaneNode::Leaf {
            pane_id: PaneId(1),
            surface: SurfaceRef::Graph(1),
        };
        for i in 0..MAX_TREE_DEPTH as u64 + 1 {
            node = PaneNode::Split {
                id: SplitId(i),
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(node),
                second: Box::new(PaneNode::Leaf {
                    pane_id: PaneId(1000 + i),
                    surface: SurfaceRef::Empty,
                }),
            };
        }
        let WorkspaceItem::Tab(tab) = &mut deep.items[0] else {
            unreachable!("the fixture starts with a tab")
        };
        tab.root = node;
        let bytes = postcard::to_stdvec(&deep).unwrap();
        assert_eq!(
            Message::decode(Kind::WorkspaceSnapshot, &bytes),
            Err(CodecError::Invalid("pane tree too deep"))
        );

        let mut many_groups = workspace();
        for i in 0..MAX_GROUPS as u64 {
            many_groups.items.push(WorkspaceItem::Group(GroupSnapshot {
                id: GroupId(100 + i),
                name: String::new(),
                color_rgba: [0; 4],
                locked: false,
                tabs: Vec::new(),
            }));
        }
        let bytes = postcard::to_stdvec(&many_groups).unwrap();
        assert_eq!(
            Message::decode(Kind::WorkspaceSnapshot, &bytes),
            Err(CodecError::Invalid("too many groups"))
        );

        let mut long_name = workspace();
        if let WorkspaceItem::Group(group) = &mut long_name.items[1] {
            group.name = "x".repeat(MAX_TITLE_BYTES + 1);
        }
        let bytes = postcard::to_stdvec(&long_name).unwrap();
        assert_eq!(
            Message::decode(Kind::WorkspaceSnapshot, &bytes),
            Err(CodecError::Invalid("group name too long"))
        );

        let mut overlap = head();
        overlap.rows[0].spans.push(CellSpan {
            start_col: 1,
            cell_count: 1,
            text: "x".into(),
            style: CellStyle::default(),
            link: None,
        });
        let bytes = postcard::to_stdvec(&overlap).unwrap();
        assert_eq!(
            Message::decode(Kind::TerminalHead, &bytes),
            Err(CodecError::Invalid("spans overlap or run backwards"))
        );

        let mut cursor = head();
        cursor.cursor.x = 80;
        let bytes = postcard::to_stdvec(&cursor).unwrap();
        assert_eq!(
            Message::decode(Kind::TerminalHead, &bytes),
            Err(CodecError::Invalid("cursor outside the grid"))
        );

        let fetch = RowFetch {
            terminal: TerminalId(3),
            epoch: 1,
            range: StableRange {
                start: 0,
                end: MAX_FETCH_ROWS as i64 + 1,
            },
            generation: 0,
        };
        let bytes = postcard::to_stdvec(&fetch).unwrap();
        assert!(Message::decode(Kind::RowFetch, &bytes).is_err());

        // The sender refuses what the receiver would: a bug on this side is
        // caught here, not by the peer ending the exchange.
        let paste = TerminalCommand::Paste {
            serial: 1,
            text: "x".repeat(MAX_TEXT_BYTES + 1),
        };
        assert_eq!(
            Message::TerminalCommand(paste).encode_body(),
            Err(CodecError::Invalid("text too long"))
        );
    }

    #[test]
    fn garbage_is_a_body_error_not_a_panic() {
        assert!(matches!(
            Message::decode(Kind::TerminalHead, &[0xff; 40]),
            Err(CodecError::Body(_))
        ));
    }
}
