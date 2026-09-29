//! The canonical screen of one terminal, and the wire views built from it.
//!
//! One `wezterm_term::Terminal` plus the things it does not model: what the
//! child's exit was, who holds the control lease, and the ordered events a
//! client must not miss even when a hundred output chunks are coalesced into
//! one notification. A [`Model`] is never shared; it lives behind the
//! session's mutex, and every mutator and every builder runs a whole turn
//! under it, so a client can never observe half a screen.
//!
//! Sequence numbers are the terminal's own. `advance_bytes`, `resize` and
//! friends bump `current_seqno` and stamp every line they touch, which is
//! exactly the damage information a delta needs; the events this module adds
//! get a fresh seqno of their own ([`Model::record`]) so that a bell or an
//! exit also moves the watch a subscriber is parked on.
//!
//! Row selection is deliberately not `Screen::get_changed_stable_rows`: that
//! treats a line at `SEQ_ZERO` as always dirty, which is right for a renderer
//! painting for the first time and wrong for a delta, where it would resend
//! every never-written blank row of the screen forever.

use std::collections::VecDeque;
use std::io::Write;

use termwiz::input::KeyboardEncoding;
use wezterm_term::{CellAttributes, CellRef, Line, Screen, Terminal, TerminalSize};
use zeughaus_mux::terminal::{MAX_COLS, MAX_LINK_BYTES, MAX_NOTIFICATION_BYTES, MAX_SPAN_BYTES};
use zeughaus_mux::{
    CellSpan, CellStyle, Controller, Cursor, Dimensions, ExitState, KeyInput, Modes, Palette,
    RowData, StableRange, TerminalDelta, TerminalEvent, TerminalHead, TerminalId,
};

use crate::config::Config;
use crate::convert;

/// The epoch a terminal starts in. A session keeps its id for its whole life,
/// but its stable rows are not one history: the primary and the alternate
/// screen number their rows independently (the alternate one from zero, and
/// it never scrolls), so each switch between them starts a new epoch. A
/// client holding rows of the other screen has to take a fresh head instead
/// of mixing the two.
pub const FIRST_EPOCH: u64 = 1;

/// Most scrollback rows a head carries above the screen. A first paint needs
/// the screen plus enough history that a small scroll does not round-trip;
/// everything further back is fetched by range.
pub const MAX_ROWS_ABOVE: usize = 512;

/// Most rows one range fetch answers with. A client asks for what it draws;
/// this bounds what a client that asks for everything gets.
pub const MAX_FETCH_ROWS: usize = 2048;

/// Most rows one delta carries: the wire's bound. A burst past it within one
/// coalescing window leaves its oldest rows for a fetch.
pub const MAX_DELTA_ROWS: usize = zeughaus_mux::codec::MAX_ROWS_PER_MESSAGE;

/// Ordered events kept. They are drained by subscribers as they advance their
/// sequence number; the bound is what a subscriber may fall behind by before
/// the oldest events are dropped, and a subscriber that far behind is resynced
/// from a fresh head anyway.
const MAX_EVENTS: usize = 1024;

pub(crate) struct Model {
    terminal: Terminal,
    /// `(seqno, event)`, oldest first. The seqno is the model's at the moment
    /// the event happened, so a delta from `seq` carries exactly the events
    /// with a greater one.
    events: VecDeque<(u64, TerminalEvent)>,
    exit: Option<ExitState>,
    controller: Option<Controller>,
    input_serial_ack: u64,
    /// What the pane is called before the child names itself.
    label: String,
    /// Whether the child ever set a title. `TerminalState` starts at the
    /// literal string "wezterm" and there is no setter, so the alternative
    /// to this flag would be shipping someone else's product name to every
    /// client until the first OSC 0.
    titled: bool,
    /// Bumped on every switch between the primary and the alternate screen.
    epoch: u64,
    /// The screen `epoch` numbers the rows of.
    alt_screen: bool,
}

impl Model {
    pub(crate) fn new(
        size: Dimensions,
        scrollback: usize,
        label: &str,
        writer: Box<dyn Write + Send>,
    ) -> Model {
        let terminal = Terminal::new(
            terminal_size(size),
            Config::new(scrollback),
            "Zeughaus",
            env!("CARGO_PKG_VERSION"),
            writer,
        );
        Model {
            terminal,
            events: VecDeque::new(),
            exit: None,
            controller: None,
            input_serial_ack: 0,
            label: label.to_string(),
            titled: false,
            epoch: FIRST_EPOCH,
            alt_screen: false,
        }
    }

    pub(crate) fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    /// The model's current sequence number: what a head is current at, and
    /// what a delta ends at.
    pub(crate) fn seq(&self) -> u64 {
        self.terminal.current_seqno() as u64
    }

    pub(crate) fn size(&self) -> Dimensions {
        let size = self.terminal.get_size();
        Dimensions {
            cols: size.cols.min(u16::MAX as usize) as u16,
            rows: size.rows.min(u16::MAX as usize) as u16,
        }
    }

    /// The stable-row space heads, deltas and pages are in.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Feeds child output. Output is the only thing that switches screens
    /// (`?1049h`/`?47h` and their resets), so the epoch is kept here.
    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        self.terminal.advance_bytes(bytes);
        let alt_screen = self.terminal.is_alt_screen_active();
        if alt_screen != self.alt_screen {
            self.alt_screen = alt_screen;
            self.epoch += 1;
        }
    }

    /// One keystroke. The legacy encodings are the terminal's own and reach
    /// the child through its writer; kitty's protocol is not something
    /// `wezterm-term` encodes, so while the child has pushed its flags the
    /// bytes come back here for the caller to write. `Ok(None)` means the
    /// terminal took care of it (or there was nothing to send).
    pub(crate) fn key(&mut self, input: KeyInput) -> Result<Option<String>, String> {
        if let KeyboardEncoding::Kitty(flags) = self.terminal.get_keyboard_encoding() {
            let bytes = convert::kitty(input, flags);
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        if let Some((code, mods)) = convert::key(input) {
            self.terminal
                .key_down(code, mods)
                .map_err(|e| e.to_string())?;
        }
        Ok(None)
    }

    /// Record an ordered event at a fresh sequence number, so that a
    /// subscriber parked on the watch wakes for it even when nothing on the
    /// screen changed.
    pub(crate) fn record(&mut self, event: TerminalEvent) -> u64 {
        self.terminal.increment_seqno();
        let seq = self.seq();
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back((seq, event));
        seq
    }

    /// The first exit wins: a child that is killed after it already exited
    /// keeps the status it exited with, and a `wait` that fails after a kill
    /// does not overwrite `Killed`.
    pub(crate) fn set_exit(&mut self, exit: ExitState) {
        if self.exit.is_some() {
            return;
        }
        self.exit = Some(exit.clone());
        self.record(TerminalEvent::Exited(exit));
    }

    pub(crate) fn exit(&self) -> Option<ExitState> {
        self.exit.clone()
    }

    /// A notification the child asked for (OSC 9 / OSC 777), each text cut
    /// to what the wire carries. An empty body with no title says nothing
    /// and is dropped.
    pub(crate) fn notify(&mut self, title: Option<String>, body: String) {
        let title = title
            .map(|t| bounded(t, MAX_NOTIFICATION_BYTES))
            .filter(|t| !t.is_empty());
        let body = bounded(body, MAX_NOTIFICATION_BYTES);
        if title.is_none() && body.is_empty() {
            return;
        }
        self.record(TerminalEvent::Notification { title, body });
    }

    pub(crate) fn set_controller(&mut self, controller: Option<Controller>) {
        self.controller = controller.clone();
        self.record(TerminalEvent::ControllerChanged(controller));
    }

    /// Remember the highest input serial applied. Serials are per client and
    /// the runner's subscriber owns the acknowledgement it sends; this is the
    /// engine's own last-applied, which is what a single-client attach needs
    /// and what a multi-client one clamps.
    pub(crate) fn note_serial(&mut self, serial: u64) {
        self.input_serial_ack = self.input_serial_ack.max(serial);
    }

    /// The child told us it changed its title; from now on that is the
    /// terminal's title.
    pub(crate) fn note_title(&mut self) {
        self.titled = true;
    }

    /// What this terminal is called: the child's OSC title once it set one,
    /// the profile's label until then.
    pub(crate) fn title(&self) -> String {
        if self.titled {
            self.terminal.get_title().to_string()
        } else {
            self.label.clone()
        }
    }

    pub(crate) fn dimensions(&self) -> Dimensions {
        let screen = self.terminal.screen();
        Dimensions {
            cols: screen.physical_cols.min(u16::MAX as usize) as u16,
            rows: screen.physical_rows.min(u16::MAX as usize) as u16,
        }
    }

    /// The whole current state a client needs, plus up to `rows_above` rows of
    /// scrollback above the screen.
    pub(crate) fn head(&self, terminal: TerminalId, rows_above: usize) -> TerminalHead {
        let screen = self.terminal.screen();
        let visible = visible_range(screen);
        let above = rows_above.min(MAX_ROWS_ABOVE) as i64;
        let mut rows = Vec::new();
        collect_rows(
            screen,
            StableRange {
                start: visible.start.saturating_sub(above),
                end: visible.end,
            },
            None,
            &mut rows,
        );
        TerminalHead {
            terminal,
            epoch: self.epoch,
            seq: self.seq(),
            dimensions: self.dimensions(),
            visible,
            first_retained: retained(screen).start,
            cursor: self.cursor(),
            title: self.title(),
            modes: self.modes(),
            palette: self.palette(),
            rows,
            exit: self.exit.clone(),
            controller: self.controller.clone(),
            input_serial_ack: self.input_serial_ack,
        }
    }

    /// Everything that changed since `seq`.
    ///
    /// Rows are reported wherever they are: a burst that scrolls a hundred
    /// lines through the screen between two deltas leaves most of them in
    /// scrollback by the time the delta is built, and a client that held
    /// those stable rows from an earlier head would otherwise keep stale
    /// content it cannot tell from current. The scan is a sequence-number
    /// comparison per retained line, which is microseconds for a full
    /// scrollback, and the rows it yields are exactly the ones written since
    /// the last delta. Above [`MAX_DELTA_ROWS`] the oldest are left out and
    /// only a fetch refreshes them. The metadata is always sent -- it is a
    /// few dozen bytes against a row's worth of text, and letting the client
    /// compare is cheaper than keeping per-subscriber copies of it here.
    pub(crate) fn delta_since(&self, terminal: TerminalId, seq: u64) -> TerminalDelta {
        let screen = self.terminal.screen();
        let visible = visible_range(screen);
        let mut row_replacements = Vec::new();
        collect_rows(screen, retained(screen), Some(seq), &mut row_replacements);
        if row_replacements.len() > MAX_DELTA_ROWS {
            let excess = row_replacements.len() - MAX_DELTA_ROWS;
            row_replacements.drain(..excess);
        }
        TerminalDelta {
            terminal,
            epoch: self.epoch,
            from_seq: seq,
            to_seq: self.seq(),
            input_serial_ack: self.input_serial_ack,
            dimensions: Some(self.dimensions()),
            visible: Some(visible),
            cursor: Some(self.cursor()),
            title: Some(self.title()),
            modes: Some(self.modes()),
            palette: Some(self.palette()),
            evicted_before: Some(retained(screen).start),
            row_replacements,
            ordered_events: self
                .events
                .iter()
                .filter(|(at, _)| *at > seq)
                .map(|(_, event)| event.clone())
                .collect(),
        }
    }

    /// The rows of `range` that are still retained, with the sequence number
    /// they were read at and the oldest row that still exists -- a client
    /// whose request was partly evicted learns how far back it may ask.
    /// `None` when the model is no longer in `epoch`: the range names rows of
    /// the other screen.
    pub(crate) fn rows(&self, epoch: u64, range: StableRange) -> Option<(u64, i64, Vec<RowData>)> {
        if epoch != self.epoch {
            return None;
        }
        let screen = self.terminal.screen();
        let end = range
            .start
            .saturating_add(MAX_FETCH_ROWS as i64)
            .min(range.end);
        let mut rows = Vec::new();
        collect_rows(
            screen,
            StableRange {
                start: range.start,
                end,
            },
            None,
            &mut rows,
        );
        Some((self.seq(), retained(screen).start, rows))
    }

    fn cursor(&self) -> Cursor {
        let screen = self.terminal.screen();
        convert::cursor(
            self.terminal.cursor_pos(),
            screen.physical_cols,
            screen.physical_rows,
        )
    }

    fn modes(&self) -> Modes {
        convert::modes(&self.terminal)
    }

    fn palette(&self) -> Palette {
        convert::palette(&self.terminal.palette())
    }
}

fn terminal_size(size: Dimensions) -> TerminalSize {
    TerminalSize {
        rows: size.rows as usize,
        cols: size.cols as usize,
        // The runner has no font and no display; a child that wants pixels
        // (sixel, kitty graphics) is out of scope for this cut anyway.
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

/// `text` cut to at most `max` bytes, at a character boundary.
fn bounded(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The stable rows currently on screen.
fn visible_range(screen: &Screen) -> StableRange {
    let start = screen.visible_row_to_stable_row(0) as i64;
    StableRange {
        start,
        end: start + screen.physical_rows as i64,
    }
}

/// The stable rows that still exist: the scrollback plus the screen.
fn retained(screen: &Screen) -> StableRange {
    let start = screen.phys_to_stable_row_index(0) as i64;
    StableRange {
        start,
        end: start + screen.scrollback_rows() as i64,
    }
}

/// Append the rows of `range` that are still retained, optionally only those
/// changed after `since`.
///
/// `Screen::with_phys_lines` would be the targeted way to do this and is not
/// usable: it intersects the requested range with both halves of the
/// scrollback `VecDeque` but then indexes the second half with the absolute
/// index, so any range reaching into it panics once the deque has wrapped
/// (`with_phys_lines_mut` has the `saturating_sub` its sibling is missing).
/// Walking the deque and filtering is a pointer bump per skipped row and
/// cannot be wrong.
fn collect_rows(screen: &Screen, range: StableRange, since: Option<u64>, out: &mut Vec<RowData>) {
    let retained = retained(screen);
    let start = range.start.clamp(retained.start, retained.end);
    let end = range.end.clamp(start, retained.end);
    if end <= start {
        return;
    }
    let phys = (start - retained.start) as usize..(end - retained.start) as usize;
    screen.for_each_phys_line(|index, line| {
        if !phys.contains(&index) {
            return;
        }
        let row_seq = line.current_seqno() as u64;
        if since.is_some_and(|since| row_seq <= since) {
            return;
        }
        out.push(row_data(retained.start + index as i64, row_seq, line));
    });
}

/// One line as spans: adjacent cells with one style and one link, trailing
/// blanks dropped.
fn row_data(stable_row: i64, row_seq: u64, line: &Line) -> RowData {
    let mut cells: Vec<Piece<'_>> = Vec::new();
    for cell in line.visible_cells() {
        let index = cell.cell_index();
        if index >= MAX_COLS as usize {
            break;
        }
        let (text, attrs) = parts(cell);
        let link = attrs
            .hyperlink()
            .map(|link| link.uri())
            // A link longer than the wire allows is dropped rather than cut:
            // half a URI is a URI to somewhere else.
            .filter(|uri| uri.len() <= MAX_LINK_BYTES);
        cells.push(Piece {
            index,
            width: cell.width(),
            text,
            style: convert::style(attrs),
            link,
        });
    }
    // The client paints the row's background behind whatever the spans do not
    // cover, so a screen full of blanks is a row full of nothing.
    while cells.last().is_some_and(Piece::is_blank) {
        cells.pop();
    }

    let mut spans: Vec<CellSpan> = Vec::new();
    for cell in cells {
        let joined = match spans.last_mut() {
            Some(span)
                if span.style == cell.style
                    && span.link.as_deref() == cell.link
                    && span.start_col as usize + span.cell_count as usize == cell.index
                    && span.text.len() + cell.text.len() <= MAX_SPAN_BYTES =>
            {
                span.text.push_str(cell.text);
                span.cell_count = span.cell_count.saturating_add(cell.width as u16);
                true
            }
            _ => false,
        };
        if !joined {
            spans.push(CellSpan {
                start_col: cell.index as u16,
                cell_count: cell.width.min(u16::MAX as usize) as u16,
                text: cell.text.to_string(),
                style: cell.style,
                link: cell.link.map(str::to_string),
            });
        }
    }

    RowData {
        stable_row,
        row_seq,
        wrapped: line.last_cell_was_wrapped(),
        spans,
    }
}

/// A cell's text and attributes, borrowed from the *line* rather than from
/// the `CellRef`.
///
/// `CellRef::str`/`attrs` elide their lifetime to `&self`, so the references
/// they hand out die with the loop variable; destructuring the enum keeps the
/// line's own lifetime and lets a row be built without copying a cell.
fn parts<'a>(cell: CellRef<'a>) -> (&'a str, &'a CellAttributes) {
    match cell {
        CellRef::CellRef { cell, .. } => (cell.str(), cell.attrs()),
        CellRef::ClusterRef { text, attrs, .. } => (text, attrs),
    }
}

struct Piece<'a> {
    index: usize,
    width: usize,
    text: &'a str,
    style: CellStyle,
    link: Option<&'a str>,
}

impl Piece<'_> {
    fn is_blank(&self) -> bool {
        self.link.is_none()
            && self.style == CellStyle::default()
            && self.text.chars().all(|c| c == ' ')
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::{Duration, Instant};

    use zeughaus_mux::{StyleFlags, WireColor};

    use super::*;

    /// A model with no PTY behind it: everything the terminal would send to
    /// a child arrives on a channel the test can read.
    ///
    /// It is a channel and not a buffer because `TerminalState` wraps the
    /// writer it is given in its own thread (so that a program filling the
    /// PTY cannot block the parser), which makes "what was sent" something a
    /// test has to wait for rather than read.
    struct Fixture {
        model: Model,
        written: Receiver<Vec<u8>>,
    }

    struct Capture(Sender<Vec<u8>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Fixture {
        fn new(cols: u16, rows: u16, scrollback: usize) -> Fixture {
            let (sender, written) = channel();
            let model = Model::new(
                Dimensions { cols, rows },
                scrollback,
                "test",
                Box::new(Capture(sender)),
            );
            Fixture { model, written }
        }

        fn feed(&mut self, bytes: &str) {
            self.model.advance(bytes.as_bytes());
        }

        fn head(&self) -> TerminalHead {
            self.model.head(TerminalId(1), MAX_ROWS_ABOVE)
        }

        fn visible_rows(&self) -> Vec<RowData> {
            let head = self.model.head(TerminalId(1), 0);
            head.rows
        }

        /// Assert that exactly `expected` reached the child, waiting for the
        /// writer thread but never longer than it can take.
        fn expect_sent(&self, expected: &[u8]) {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut sent = Vec::new();
            while sent.len() < expected.len() {
                match self
                    .written
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(chunk) => sent.extend_from_slice(&chunk),
                    Err(_) => break,
                }
            }
            assert_eq!(sent, expected);
        }
    }

    fn text_of(row: &RowData) -> String {
        row.spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn the_profile_label_stands_in_until_the_child_names_itself() {
        let mut fixture = Fixture::new(20, 3, 10);
        assert_eq!(fixture.model.title(), "test");

        fixture.feed("\x1b]0;vim README\x07");
        // The session learns from the terminal's alert that a title was set;
        // before that, whatever the terminal core defaults to is not ours to
        // show.
        fixture.model.note_title();
        assert_eq!(fixture.model.title(), "vim README");
        assert_eq!(fixture.head().title, "vim README");
    }

    #[test]
    fn spans_carry_the_attributes_that_produced_them() {
        let mut fixture = Fixture::new(20, 3, 10);
        fixture.feed("\x1b[1;31mab\x1b[0m c");

        let rows = fixture.visible_rows();
        let spans = &rows[0].spans;
        assert_eq!(spans.len(), 2, "one span per style run: {spans:?}");

        assert_eq!(spans[0].text, "ab");
        assert_eq!(spans[0].start_col, 0);
        assert_eq!(spans[0].cell_count, 2);
        assert!(spans[0].style.flags.has(StyleFlags::BOLD));
        assert_eq!(spans[0].style.fg, WireColor::Indexed(1));

        // The space is part of the default run, not dropped: only trailing
        // blanks are, and this one has a `c` after it.
        assert_eq!(spans[1].text, " c");
        assert_eq!(spans[1].start_col, 2);
        assert_eq!(spans[1].style, CellStyle::default());
    }

    #[test]
    fn a_wide_glyph_is_two_cells_and_a_combining_mark_is_none() {
        let mut fixture = Fixture::new(20, 3, 10);
        fixture.feed("a\u{4e2d}b");
        let rows = fixture.visible_rows();
        assert_eq!(rows[0].spans.len(), 1);
        assert_eq!(text_of(&rows[0]), "a\u{4e2d}b");
        // Four columns for three graphemes: the wide one occupies two.
        assert_eq!(rows[0].spans[0].cell_count, 4);

        let mut fixture = Fixture::new(20, 3, 10);
        fixture.feed("e\u{301}");
        let rows = fixture.visible_rows();
        assert_eq!(text_of(&rows[0]), "e\u{301}");
        assert_eq!(rows[0].spans[0].cell_count, 1);
    }

    #[test]
    fn the_alternate_screen_hides_the_primary_one_and_gives_it_back() {
        let mut fixture = Fixture::new(20, 3, 10);
        fixture.feed("primary");
        assert!(!fixture.head().modes.alt_screen);

        fixture.feed("\x1b[?1049h");
        let head = fixture.head();
        assert!(head.modes.alt_screen);
        assert!(
            head.rows
                .iter()
                .all(|row| !text_of(row).contains("primary")),
            "the alternate screen starts blank"
        );

        fixture.feed("\x1b[?1049l");
        let head = fixture.head();
        assert!(!head.modes.alt_screen);
        assert!(
            head.rows.iter().any(|row| text_of(row).contains("primary")),
            "leaving restores the primary screen"
        );
    }

    #[test]
    fn switching_screens_starts_a_new_epoch() {
        let mut fixture = Fixture::new(20, 3, 5);
        for line in 0..20 {
            fixture.feed(&format!("line {line}\r\n"));
        }
        let primary = fixture.head();
        assert_eq!(primary.epoch, FIRST_EPOCH);
        assert!(primary.first_retained > 0, "the scrollback evicted rows");

        fixture.feed("\x1b[?1049h");
        let alternate = fixture.head();
        assert_ne!(alternate.epoch, primary.epoch);
        // The alternate screen numbers its rows from zero: below what the
        // primary one has already evicted, so the two cannot share an epoch.
        assert!(alternate.visible.start < primary.first_retained);
        let delta = fixture.model.delta_since(TerminalId(1), primary.seq);
        assert!(!delta.applies_to(primary.epoch, primary.seq));
        let range = StableRange {
            start: primary.first_retained,
            end: primary.visible.end,
        };
        assert!(fixture.model.rows(primary.epoch, range).is_none());

        fixture.feed("\x1b[?1049l");
        let back = fixture.head();
        assert_ne!(back.epoch, alternate.epoch);
        assert!(fixture.model.rows(back.epoch, range).is_some());
    }

    #[test]
    fn narrowing_the_grid_rewraps_and_reports_the_rows_it_changed() {
        let mut fixture = Fixture::new(10, 3, 20);
        fixture.feed("aaaaaaaaaa\r\nbbb\r\nccc");
        let before = fixture.model.seq();

        fixture.model.terminal_mut().resize(TerminalSize {
            rows: 3,
            cols: 5,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        });

        let head = fixture.head();
        assert_eq!(head.dimensions, Dimensions { cols: 5, rows: 3 });
        let wrapped: Vec<&RowData> = head.rows.iter().filter(|row| row.wrapped).collect();
        assert_eq!(
            wrapped.len(),
            1,
            "the ten-column line is now two rows: {:?}",
            head.rows
        );
        assert_eq!(text_of(wrapped[0]), "aaaaa");

        let delta = fixture.model.delta_since(TerminalId(1), before);
        assert!(delta.to_seq > before);
        let changed: Vec<i64> = delta
            .row_replacements
            .iter()
            .map(|row| row.stable_row)
            .collect();
        assert!(
            !changed.is_empty(),
            "a rewrap is damage the client has to be told about"
        );
        assert_eq!(delta.dimensions, Some(Dimensions { cols: 5, rows: 3 }));
    }

    #[test]
    fn a_delta_carries_exactly_the_rows_that_changed() {
        let mut fixture = Fixture::new(20, 4, 10);
        fixture.feed("first\r\n");
        let head = fixture.head();
        let seq = head.seq;

        fixture.feed("second");
        let delta = fixture.model.delta_since(TerminalId(1), seq);

        assert!(delta.to_seq > seq);
        assert_eq!(delta.from_seq, seq);
        assert_eq!(
            delta.row_replacements.len(),
            1,
            "only the second line moved: {:?}",
            delta.row_replacements
        );
        assert_eq!(text_of(&delta.row_replacements[0]), "second");
        assert!(delta.applies_to(FIRST_EPOCH, seq));

        // Nothing happened since: a client at `to_seq` gets no rows at all.
        let idle = fixture.model.delta_since(TerminalId(1), delta.to_seq);
        assert!(idle.row_replacements.is_empty());
    }

    #[test]
    fn scrollback_eviction_moves_the_oldest_retained_row() {
        let mut fixture = Fixture::new(20, 3, 5);
        let head = fixture.head();
        assert_eq!(head.first_retained, 0);
        let seq = head.seq;

        for line in 0..20 {
            fixture.feed(&format!("line {line}\r\n"));
        }

        let head = fixture.head();
        assert!(
            head.first_retained > 0,
            "five rows of scrollback cannot hold twenty lines"
        );
        // Screen plus scrollback, and not one row more.
        assert_eq!(head.visible.end - head.first_retained, 3 + 5);

        let delta = fixture.model.delta_since(TerminalId(1), seq);
        assert_eq!(delta.evicted_before, Some(head.first_retained));
        assert!(
            delta
                .row_replacements
                .iter()
                .all(|row| row.stable_row >= head.first_retained),
            "an evicted row cannot be replaced"
        );

        // A range fetch that runs off both ends answers with what exists and
        // says where that starts.
        let (_, first_retained, rows) = fixture
            .model
            .rows(
                FIRST_EPOCH,
                StableRange {
                    start: -5,
                    end: 1_000,
                },
            )
            .expect("still the primary screen");
        assert_eq!(first_retained, head.first_retained);
        assert_eq!(rows.len(), 3 + 5);
        assert_eq!(rows[0].stable_row, first_retained);
    }

    #[test]
    fn a_scrolled_back_viewer_still_learns_about_the_screen() {
        let mut fixture = Fixture::new(20, 3, 40);
        for line in 0..10 {
            fixture.feed(&format!("line {line}\r\n"));
        }
        let seq = fixture.head().seq;
        fixture.feed("fresh");

        // The viewer is looking at the top of the scrollback, nowhere near
        // the screen; the screen is current for it anyway.
        let delta = fixture.model.delta_since(TerminalId(1), seq);
        assert!(
            delta
                .row_replacements
                .iter()
                .any(|row| text_of(row) == "fresh"),
            "the visible change is missing: {:?}",
            delta.row_replacements
        );
    }

    fn press(key: zeughaus_mux::input::Key, modifiers: u8) -> KeyInput {
        KeyInput {
            key,
            modifiers: zeughaus_mux::Modifiers(modifiers),
            kind: zeughaus_mux::KeyKind::Press,
        }
    }

    const SHIFT: u8 = zeughaus_mux::Modifiers::SHIFT;
    const CTRL: u8 = zeughaus_mux::Modifiers::CTRL;

    fn named(key: zeughaus_mux::NamedKey) -> zeughaus_mux::input::Key {
        zeughaus_mux::input::Key::Named(key)
    }

    #[test]
    fn keys_are_encoded_where_the_modes_are_known() {
        use zeughaus_mux::NamedKey::{Enter, Up};
        use zeughaus_mux::input::Key::Char;
        let mut fixture = Fixture::new(20, 3, 10);
        let legacy = |fixture: &mut Fixture, input: KeyInput, expected: &[u8]| {
            assert_eq!(fixture.model.key(input), Ok(None));
            fixture.expect_sent(expected);
        };

        legacy(&mut fixture, press(named(Up), 0), b"\x1b[A");
        // DECCKM: the same key, a different sequence, and the client never
        // had to know.
        fixture.feed("\x1b[?1h");
        legacy(&mut fixture, press(named(Up), 0), b"\x1bOA");
        legacy(&mut fixture, press(Char('c'), CTRL), b"\x03");
        legacy(&mut fixture, press(named(Enter), 0), b"\r");
        // xterm cannot tell Shift+Enter from Enter; a line feed can.
        legacy(&mut fixture, press(named(Enter), SHIFT), b"\n");
        let release = KeyInput {
            kind: zeughaus_mux::KeyKind::Release,
            ..press(Char('a'), 0)
        };
        assert_eq!(fixture.model.key(release), Ok(None));
        legacy(&mut fixture, press(Char('x'), 0), b"x");
    }

    #[test]
    fn a_child_that_pushes_kitty_flags_gets_kitty_keys() {
        use zeughaus_mux::NamedKey::Enter;
        use zeughaus_mux::input::Key::Char;
        let mut fixture = Fixture::new(20, 3, 10);
        // Support is advertised: the query is answered with the flags in
        // force, none yet.
        fixture.feed("\x1b[?u");
        fixture.expect_sent(b"\x1b[?0u");

        // Disambiguate escape codes.
        fixture.feed("\x1b[>1u");
        let kitty = |fixture: &mut Fixture, input: KeyInput| fixture.model.key(input).unwrap();
        assert_eq!(
            kitty(&mut fixture, press(named(Enter), SHIFT)).as_deref(),
            Some("\x1b[13;2u")
        );
        assert_eq!(
            kitty(&mut fixture, press(Char('c'), CTRL)).as_deref(),
            Some("\x1b[99;5u")
        );
        assert_eq!(
            kitty(&mut fixture, press(Char('a'), 0)).as_deref(),
            Some("a")
        );
        let release = KeyInput {
            kind: zeughaus_mux::KeyKind::Release,
            ..press(Char('a'), 0)
        };
        assert_eq!(kitty(&mut fixture, release), None, "no event types asked");

        // Disambiguate plus event types: releases are reported.
        fixture.feed("\x1b[>3u");
        assert_eq!(
            kitty(&mut fixture, release).as_deref(),
            Some("\x1b[97;1:3u")
        );

        // Popping both pushes is the legacy encoding again.
        fixture.feed("\x1b[<2u");
        assert_eq!(kitty(&mut fixture, press(named(Enter), SHIFT)), None);
        fixture.expect_sent(b"\n");
    }

    #[test]
    fn ordered_events_survive_coalescing() {
        let mut fixture = Fixture::new(20, 3, 10);
        let seq = fixture.model.seq();
        fixture.model.record(TerminalEvent::Bell);
        fixture.model.set_exit(ExitState::Exited { code: 3 });

        let delta = fixture.model.delta_since(TerminalId(1), seq);
        assert_eq!(
            delta.ordered_events,
            vec![
                TerminalEvent::Bell,
                TerminalEvent::Exited(ExitState::Exited { code: 3 })
            ]
        );
        assert_eq!(fixture.model.exit(), Some(ExitState::Exited { code: 3 }));
        // A second exit does not rewrite the first.
        fixture.model.set_exit(ExitState::Killed);
        assert_eq!(fixture.model.exit(), Some(ExitState::Exited { code: 3 }));
    }

    #[test]
    fn a_notification_is_cut_to_what_the_wire_carries() {
        let mut fixture = Fixture::new(20, 3, 10);
        let seq = fixture.model.seq();
        fixture.model.notify(None, String::new());
        // Two bytes per character: the limit falls inside one.
        fixture.model.notify(Some(String::new()), "ä".repeat(600));

        let delta = fixture.model.delta_since(TerminalId(1), seq);
        let [TerminalEvent::Notification { title, body }] = delta.ordered_events.as_slice() else {
            panic!("one notification: {:?}", delta.ordered_events);
        };
        assert_eq!(*title, None, "an empty title is no title");
        assert_eq!(body.len(), MAX_NOTIFICATION_BYTES);
        assert!(body.chars().all(|c| c == 'ä'));
    }

    #[test]
    fn a_lease_change_is_ordered_and_visible_in_the_head() {
        let mut fixture = Fixture::new(20, 3, 10);
        let seq = fixture.model.seq();
        let controller = Controller {
            client: zeughaus_mux::ClientInstanceId::from_bytes([7; 16]),
            principal: "sha256:abcd".to_string(),
        };
        fixture.model.set_controller(Some(controller.clone()));

        assert!(fixture.model.seq() > seq, "a takeover wakes a subscriber");
        let delta = fixture.model.delta_since(TerminalId(1), seq);
        assert_eq!(
            delta.ordered_events,
            vec![TerminalEvent::ControllerChanged(Some(controller.clone()))]
        );
        assert_eq!(fixture.head().controller, Some(controller));
    }
}
