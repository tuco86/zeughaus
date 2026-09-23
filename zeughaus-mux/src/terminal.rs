//! What a terminal looks like on the wire: its head, its rows, and the deltas
//! that keep a client's copy current.
//!
//! The runner parses PTY bytes once and every client receives canonical row
//! state; raw output is never the remote rendering contract. Rows are
//! addressed by **stable row index** -- a number that does not change when the
//! screen scrolls, only when the scrollback evicts the row -- and every row
//! carries the sequence number of its last change, so a delta can name
//! exactly the rows a client has to replace.
//!
//! A client applies a [`TerminalDelta`] only when it holds the same
//! `epoch` and its applied sequence equals `from_seq`. Any gap means a fresh
//! [`TerminalHead`]; nothing is patched speculatively.

use serde::{Deserialize, Serialize};

use crate::id::{ClientInstanceId, TerminalId};

/// Columns and rows of the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimensions {
    pub cols: u16,
    pub rows: u16,
}

/// Largest grid either side accepts. A 4K display at a 6 px font is about
/// 640 x 360; the bound leaves room for that and refuses a request that
/// would make the runner allocate a screen nobody can show.
pub const MAX_COLS: u16 = 1024;
pub const MAX_ROWS: u16 = 512;

impl Dimensions {
    /// Whether both sides are within bounds and neither is zero.
    pub fn is_valid(self) -> bool {
        (1..=MAX_COLS).contains(&self.cols) && (1..=MAX_ROWS).contains(&self.rows)
    }
}

/// A contiguous range of stable rows, `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableRange {
    pub start: i64,
    pub end: i64,
}

impl StableRange {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start).max(0) as u64
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn contains(&self, row: i64) -> bool {
        row >= self.start && row < self.end
    }
}

/// Where the cursor is and how it is drawn. `x`/`y` are grid coordinates in
/// the visible screen, `y` counted from the top visible row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub visible: bool,
    pub blinking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// Terminal modes a renderer or an input router has to know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modes {
    pub alt_screen: bool,
    /// Any mouse reporting mode is on: the controller's mouse goes to the
    /// child instead of selecting text.
    pub mouse_reporting: bool,
    pub bracketed_paste: bool,
    pub reverse_video: bool,
    /// Focus in/out reporting is on: the controller's focus changes are
    /// forwarded.
    pub focus_reporting: bool,
}

/// A colour as the terminal states it. Indexed colours are resolved against
/// the [`Palette`] on the client, so a palette change repaints without
/// resending rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireColor {
    Default,
    Indexed(u8),
    Rgb([u8; 3]),
}

/// The 16 ANSI colours plus the defaults, as the terminal currently has
/// them. The 256-colour cube and greys are fixed by convention and computed
/// on the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    pub ansi: [[u8; 3]; 16],
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: [u8; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            ansi: [
                [0x00, 0x00, 0x00],
                [0xcc, 0x00, 0x00],
                [0x4e, 0x9a, 0x06],
                [0xc4, 0xa0, 0x00],
                [0x34, 0x65, 0xa4],
                [0x75, 0x50, 0x7b],
                [0x06, 0x98, 0x9a],
                [0xd3, 0xd7, 0xcf],
                [0x55, 0x57, 0x53],
                [0xef, 0x29, 0x29],
                [0x8a, 0xe2, 0x34],
                [0xfc, 0xe9, 0x4f],
                [0x72, 0x9f, 0xcf],
                [0xad, 0x7f, 0xa8],
                [0x34, 0xe2, 0xe2],
                [0xee, 0xee, 0xec],
            ],
            foreground: [0xd3, 0xd7, 0xcf],
            background: [0x1c, 0x1c, 0x1c],
            cursor: [0xd3, 0xd7, 0xcf],
        }
    }
}

impl Palette {
    /// What a terminal reports before anything changed it: the colours the
    /// runner's terminal core (`wezterm-term`'s `ColorPalette::default()`)
    /// starts with. It is the marker for "this entry is still the default",
    /// which is what [`Palette::themed`] needs in order to tell an untouched
    /// slot from one an `OSC 4`/`10`/`11` deliberately set.
    pub const RUNNER_DEFAULT: Palette = Palette {
        ansi: [
            [0x00, 0x00, 0x00],
            [0xcc, 0x55, 0x55],
            [0x55, 0xcc, 0x55],
            [0xcd, 0xcd, 0x55],
            [0x54, 0x55, 0xcb],
            [0xcc, 0x55, 0xcc],
            [0x7a, 0xca, 0xca],
            [0xcc, 0xcc, 0xcc],
            [0x55, 0x55, 0x55],
            [0xff, 0x55, 0x55],
            [0x55, 0xff, 0x55],
            [0xff, 0xff, 0x55],
            [0x55, 0x55, 0xff],
            [0xff, 0x55, 0xff],
            [0x55, 0xff, 0xff],
            [0xff, 0xff, 0xff],
        ],
        // Grey70 out of the 24-step grey ramp, the core's foreground.
        foreground: [0xb2, 0xb2, 0xb2],
        background: [0x00, 0x00, 0x00],
        cursor: [0x52, 0xad, 0x70],
    };

    /// This palette with every still-default entry taken from `theme`.
    ///
    /// The child owns its colours: once it set one through `OSC 4`/`10`/`11`
    /// that colour is what it asked for and a theme must not override it.
    /// Every entry still equal to [`Palette::RUNNER_DEFAULT`] was never set
    /// and is the client's to choose.
    pub fn themed(&self, theme: &Palette) -> Palette {
        let mut out = self.clone();
        for (index, slot) in out.ansi.iter_mut().enumerate() {
            if *slot == Palette::RUNNER_DEFAULT.ansi[index] {
                *slot = theme.ansi[index];
            }
        }
        if out.foreground == Palette::RUNNER_DEFAULT.foreground {
            out.foreground = theme.foreground;
        }
        if out.background == Palette::RUNNER_DEFAULT.background {
            out.background = theme.background;
        }
        if out.cursor == Palette::RUNNER_DEFAULT.cursor {
            out.cursor = theme.cursor;
        }
        out
    }
}

/// Underline style, three bits of [`StyleFlags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Underline {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Cell attributes as flag bits, so a span's style compares and hashes as one
/// word. `underline` is packed into bits 8..11 through [`StyleFlags::with_underline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct StyleFlags(pub u16);

impl StyleFlags {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const STRIKETHROUGH: u16 = 1 << 3;
    pub const BLINK: u16 = 1 << 4;
    pub const REVERSE: u16 = 1 << 5;
    pub const INVISIBLE: u16 = 1 << 6;
    pub const OVERLINE: u16 = 1 << 7;
    /// The span carries a hyperlink; the target is in [`CellSpan::link`].
    pub const HYPERLINK: u16 = 1 << 11;
    const UNDERLINE_SHIFT: u16 = 8;
    const UNDERLINE_MASK: u16 = 0b111 << Self::UNDERLINE_SHIFT;

    pub fn has(self, flag: u16) -> bool {
        self.0 & flag != 0
    }

    pub fn with(self, flag: u16) -> Self {
        StyleFlags(self.0 | flag)
    }

    pub fn with_underline(self, underline: Underline) -> Self {
        let bits = match underline {
            Underline::None => 0,
            Underline::Single => 1,
            Underline::Double => 2,
            Underline::Curly => 3,
            Underline::Dotted => 4,
            Underline::Dashed => 5,
        };
        StyleFlags((self.0 & !Self::UNDERLINE_MASK) | (bits << Self::UNDERLINE_SHIFT))
    }

    pub fn underline(self) -> Underline {
        match (self.0 & Self::UNDERLINE_MASK) >> Self::UNDERLINE_SHIFT {
            1 => Underline::Single,
            2 => Underline::Double,
            3 => Underline::Curly,
            4 => Underline::Dotted,
            5 => Underline::Dashed,
            _ => Underline::None,
        }
    }
}

/// The wire-stable style of one span of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellStyle {
    pub fg: WireColor,
    pub bg: WireColor,
    pub underline_color: WireColor,
    pub flags: StyleFlags,
}

impl Default for CellStyle {
    fn default() -> Self {
        CellStyle {
            fg: WireColor::Default,
            bg: WireColor::Default,
            underline_color: WireColor::Default,
            flags: StyleFlags::default(),
        }
    }
}

/// Adjacent cells with one style. `cell_count` is the width in grid cells,
/// independent of `text`'s byte or scalar length: a wide glyph is one
/// scalar and two cells, a combining sequence is several scalars and one
/// cell. Blank runs at the end of a row are omitted; a client draws the
/// row's background there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSpan {
    pub start_col: u16,
    pub cell_count: u16,
    pub text: String,
    pub style: CellStyle,
    /// The hyperlink target when [`StyleFlags::HYPERLINK`] is set. Never
    /// opened by the client without an explicit action.
    pub link: Option<String>,
}

/// Longest `text` of one span. A span never exceeds a row, and a row never
/// exceeds [`MAX_COLS`] cells of at most a few scalars each.
pub const MAX_SPAN_BYTES: usize = 16 * 1024;

/// Longest hyperlink target kept.
pub const MAX_LINK_BYTES: usize = 2048;

/// One row's content at one sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowData {
    pub stable_row: i64,
    /// Sequence number of the last change to this row.
    pub row_seq: u64,
    /// The row continues on the next one: a logical line the grid wrapped,
    /// which selection and rewrap treat as one.
    pub wrapped: bool,
    pub spans: Vec<CellSpan>,
}

/// Why a child is no longer running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitState {
    Exited {
        code: u32,
    },
    Signaled {
        signal: String,
    },
    /// The runner killed it: an explicit close, or a spawn that failed
    /// after the terminal was announced.
    Killed,
    /// The child could not be started at all.
    SpawnFailed {
        reason: String,
    },
}

/// Who may type into a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Controller {
    pub client: ClientInstanceId,
    /// What the client proved to the runner, as the runner shows it to the
    /// others: a fingerprint's text, or a name from a certificate.
    pub principal: String,
}

/// A terminal's whole current state as far as a client needs it: sent on
/// attach, and again whenever a delta cannot be applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalHead {
    pub terminal: TerminalId,
    /// Bumped by the runner when the terminal's history is discontinuous
    /// with what any client may hold (a session restart on the same id
    /// never happens, so today this is always the first epoch; the field
    /// exists so that stays a rule of the runner and not of the wire).
    pub epoch: u64,
    /// The sequence number this head is current at. Deltas continue from
    /// here.
    pub seq: u64,
    pub dimensions: Dimensions,
    /// The stable rows currently on screen.
    pub visible: StableRange,
    /// The oldest stable row still retained: everything before it was
    /// evicted from scrollback and cannot be fetched.
    pub first_retained: i64,
    pub cursor: Cursor,
    pub title: String,
    pub modes: Modes,
    pub palette: Palette,
    /// The visible rows, plus up to a bounded window of scrollback above
    /// them. A row absent from here is fetched on demand ([`crate::RowFetch`]).
    pub rows: Vec<RowData>,
    pub exit: Option<ExitState>,
    pub controller: Option<Controller>,
    /// The highest input serial from this client the runner has applied.
    pub input_serial_ack: u64,
}

/// Something that happened in order and must not be coalesced away: an exit,
/// a bell, a lease change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalEvent {
    Exited(ExitState),
    Bell,
    ControllerChanged(Option<Controller>),
}

/// The change between two sequence numbers of one terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalDelta {
    pub terminal: TerminalId,
    pub epoch: u64,
    /// The sequence number the receiver must be at.
    pub from_seq: u64,
    /// The sequence number it is at after applying this.
    pub to_seq: u64,
    pub input_serial_ack: u64,
    pub dimensions: Option<Dimensions>,
    pub visible: Option<StableRange>,
    pub cursor: Option<Cursor>,
    pub title: Option<String>,
    pub modes: Option<Modes>,
    pub palette: Option<Palette>,
    /// Rows before this stable index were evicted; the client drops them.
    pub evicted_before: Option<i64>,
    pub row_replacements: Vec<RowData>,
    pub ordered_events: Vec<TerminalEvent>,
}

impl TerminalDelta {
    /// Whether a client at `epoch`/`applied_seq` may apply this delta.
    pub fn applies_to(&self, epoch: u64, applied_seq: u64) -> bool {
        self.epoch == epoch && self.from_seq == applied_seq && self.to_seq >= self.from_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underline_packs_into_the_flags_word() {
        let flags = StyleFlags::default()
            .with(StyleFlags::BOLD)
            .with_underline(Underline::Curly);
        assert!(flags.has(StyleFlags::BOLD));
        assert_eq!(flags.underline(), Underline::Curly);
        let none = flags.with_underline(Underline::None);
        assert_eq!(none.underline(), Underline::None);
        assert!(
            none.has(StyleFlags::BOLD),
            "clearing the underline keeps the rest"
        );
    }

    #[test]
    fn a_delta_applies_only_at_its_base() {
        let delta = TerminalDelta {
            terminal: TerminalId(1),
            epoch: 1,
            from_seq: 10,
            to_seq: 12,
            input_serial_ack: 0,
            dimensions: None,
            visible: None,
            cursor: None,
            title: None,
            modes: None,
            palette: None,
            evicted_before: None,
            row_replacements: vec![],
            ordered_events: vec![],
        };
        assert!(delta.applies_to(1, 10));
        assert!(!delta.applies_to(1, 9), "a gap needs a fresh head");
        assert!(!delta.applies_to(1, 11), "already past it");
        assert!(!delta.applies_to(2, 10), "another epoch is another history");
    }

    #[test]
    fn dimensions_are_bounded_and_nonzero() {
        assert!(Dimensions { cols: 80, rows: 24 }.is_valid());
        assert!(!Dimensions { cols: 0, rows: 24 }.is_valid());
        assert!(
            !Dimensions {
                cols: MAX_COLS + 1,
                rows: 1
            }
            .is_valid()
        );
    }

    #[test]
    fn a_theme_fills_the_untouched_slots_only() {
        let theme = Palette {
            ansi: [[9, 9, 9]; 16],
            foreground: [1, 1, 1],
            background: [2, 2, 2],
            cursor: [3, 3, 3],
        };

        let fresh = Palette::RUNNER_DEFAULT.themed(&theme);
        assert_eq!(fresh, theme, "an untouched palette becomes the theme");

        // What an OSC 4/10/11 left behind stays, in every kind of slot.
        let mut changed = Palette::RUNNER_DEFAULT;
        changed.ansi[1] = [200, 100, 50];
        changed.background = [20, 20, 20];
        let mixed = changed.themed(&theme);
        assert_eq!(mixed.ansi[1], [200, 100, 50]);
        assert_eq!(mixed.background, [20, 20, 20]);
        assert_eq!(mixed.ansi[0], theme.ansi[0], "the rest still follows");
        assert_eq!(mixed.foreground, theme.foreground);
        assert_eq!(mixed.cursor, theme.cursor);
    }
}
