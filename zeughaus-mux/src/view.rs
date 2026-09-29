//! A client's copy of one terminal: what the runner sent, applied in order,
//! bounded in what it keeps.
//!
//! Pure state so the editor's transport task and its renderer share one
//! definition without either owning it: heads replace everything, deltas
//! apply only at their exact base, pages fill rows the client scrolled to,
//! and the row store never grows past `capacity` -- rows farthest from the
//! viewport go first. Everything a renderer draws is read from here;
//! everything the transport learns is written here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::id::TerminalId;
use crate::message::RowPage;
use crate::terminal::{
    Controller, Cursor, CursorShape, Dimensions, ExitState, Modes, Palette, RowData, StableRange,
    TerminalDelta, TerminalEvent, TerminalHead,
};

/// Why a delta was not applied. Either way the caller asks for a fresh
/// head; the distinction is for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// Another terminal's delta reached this view.
    WrongTerminal,
    /// The terminal's history restarted.
    Epoch { held: u64, delta: u64 },
    /// The delta does not start where this view is.
    Gap { applied: u64, from: u64 },
}

/// The client's copy of one terminal.
#[derive(Debug, Clone)]
pub struct TerminalView {
    pub terminal: TerminalId,
    pub epoch: u64,
    pub applied_seq: u64,
    pub dimensions: Dimensions,
    /// The rows the runner's screen shows.
    pub visible: StableRange,
    pub first_retained: i64,
    pub cursor: Cursor,
    pub title: String,
    pub modes: Modes,
    pub palette: Palette,
    pub exit: Option<ExitState>,
    pub controller: Option<Controller>,
    pub input_serial_ack: u64,
    /// The top row this client shows when scrolled into history; `None`
    /// follows the screen.
    pub scroll_top: Option<i64>,
    /// Bumped on every change a renderer would have to redraw for.
    pub render_revision: u64,
    rows: BTreeMap<i64, RowData>,
    capacity: usize,
    dirty: BTreeSet<i64>,
    events: VecDeque<TerminalEvent>,
    /// The generation of the newest fetch issued; a page from an older one
    /// may still fill rows nothing newer touched.
    fetch_generation: u64,
}

impl TerminalView {
    /// A view from a fresh head, keeping at most `capacity` rows.
    pub fn from_head(head: TerminalHead, capacity: usize) -> TerminalView {
        let mut view = TerminalView {
            terminal: head.terminal,
            epoch: head.epoch,
            applied_seq: head.seq,
            dimensions: head.dimensions,
            visible: head.visible,
            first_retained: head.first_retained,
            cursor: head.cursor,
            title: head.title,
            modes: head.modes,
            palette: head.palette,
            exit: head.exit,
            controller: head.controller,
            input_serial_ack: head.input_serial_ack,
            scroll_top: None,
            render_revision: 1,
            rows: BTreeMap::new(),
            capacity: capacity.max(1),
            dirty: BTreeSet::new(),
            events: VecDeque::new(),
            fetch_generation: 0,
        };
        for row in head.rows {
            view.dirty.insert(row.stable_row);
            view.rows.insert(row.stable_row, row);
        }
        view.trim();
        view
    }

    /// Replaces everything with a fresh head. The scroll position is kept
    /// when the rows it names still exist, so a resync under the user does
    /// not jump them to the bottom.
    pub fn apply_head(&mut self, head: TerminalHead) {
        let scroll_top = self.scroll_top;
        let capacity = self.capacity;
        let fetch_generation = self.fetch_generation;
        *self = TerminalView::from_head(head, capacity);
        self.fetch_generation = fetch_generation;
        self.scroll_top = scroll_top;
        self.clamp_scroll();
    }

    /// Applies a delta, or says why not. A rejection changes nothing.
    pub fn apply_delta(&mut self, delta: TerminalDelta) -> Result<(), Rejected> {
        if delta.terminal != self.terminal {
            return Err(Rejected::WrongTerminal);
        }
        if delta.epoch != self.epoch {
            return Err(Rejected::Epoch {
                held: self.epoch,
                delta: delta.epoch,
            });
        }
        if !delta.applies_to(self.epoch, self.applied_seq) {
            return Err(Rejected::Gap {
                applied: self.applied_seq,
                from: delta.from_seq,
            });
        }
        self.applied_seq = delta.to_seq;
        self.input_serial_ack = self.input_serial_ack.max(delta.input_serial_ack);
        let mut changed = false;
        if let Some(d) = delta.dimensions
            && d != self.dimensions
        {
            self.dimensions = d;
            changed = true;
        }
        if let Some(v) = delta.visible
            && v != self.visible
        {
            self.visible = v;
            changed = true;
        }
        if let Some(c) = delta.cursor
            && c != self.cursor
        {
            self.cursor = c;
            changed = true;
        }
        if let Some(t) = delta.title
            && t != self.title
        {
            self.title = t;
            changed = true;
        }
        if let Some(m) = delta.modes
            && m != self.modes
        {
            self.modes = m;
            changed = true;
        }
        if let Some(p) = delta.palette
            && p != self.palette
        {
            self.palette = p;
            changed = true;
        }
        if let Some(before) = delta.evicted_before {
            self.first_retained = self.first_retained.max(before);
            let evicted: Vec<i64> = self.rows.range(..before).map(|(k, _)| *k).collect();
            for key in evicted {
                self.rows.remove(&key);
                self.dirty.remove(&key);
            }
            changed |= self.clamp_scroll();
        }
        for row in delta.row_replacements {
            self.dirty.insert(row.stable_row);
            self.rows.insert(row.stable_row, row);
            changed = true;
        }
        for event in delta.ordered_events {
            match &event {
                TerminalEvent::Exited(state) => {
                    self.exit = Some(state.clone());
                    changed = true;
                }
                TerminalEvent::ControllerChanged(controller) => {
                    self.controller = controller.clone();
                    changed = true;
                }
                TerminalEvent::Bell | TerminalEvent::Notification { .. } => {}
            }
            self.events.push_back(event);
        }
        self.trim();
        if changed {
            self.render_revision += 1;
        }
        Ok(())
    }

    /// Fills rows from a fetched page. Rejected when the page is another
    /// terminal's or epoch's. A row already held at a newer sequence than
    /// the page is not overwritten: the page answered an older question.
    pub fn apply_page(&mut self, page: RowPage) -> bool {
        if page.terminal != self.terminal || page.epoch != self.epoch {
            return false;
        }
        self.first_retained = self.first_retained.max(page.first_retained);
        let mut changed = false;
        for row in page.rows {
            if row.stable_row < self.first_retained {
                continue;
            }
            let newer_held = self
                .rows
                .get(&row.stable_row)
                .is_some_and(|held| held.row_seq > row.row_seq);
            if newer_held {
                continue;
            }
            self.dirty.insert(row.stable_row);
            self.rows.insert(row.stable_row, row);
            changed = true;
        }
        self.trim();
        if changed {
            self.render_revision += 1;
        }
        changed
    }

    /// The next fetch generation, for a [`crate::RowFetch`].
    pub fn next_fetch_generation(&mut self) -> u64 {
        self.fetch_generation += 1;
        self.fetch_generation
    }

    pub fn row(&self, stable_row: i64) -> Option<&RowData> {
        self.rows.get(&stable_row)
    }

    /// Rows held, for a renderer that wants to iterate the viewport.
    pub fn rows_in(&self, range: StableRange) -> impl Iterator<Item = &RowData> {
        self.rows.range(range.start..range.end).map(|(_, row)| row)
    }

    /// The rows this client shows: the screen, or `dimensions.rows` rows
    /// from `scroll_top`.
    pub fn viewport(&self) -> StableRange {
        match self.scroll_top {
            None => self.visible,
            Some(top) => StableRange {
                start: top,
                end: top + i64::from(self.dimensions.rows),
            },
        }
    }

    /// Whether the client is looking at the live screen.
    pub fn follows_screen(&self) -> bool {
        self.scroll_top.is_none()
    }

    /// Scrolls the viewport by `lines` (negative is up, into history).
    /// Returns whether it moved.
    pub fn scroll_by(&mut self, lines: i64) -> bool {
        let current = self.viewport().start;
        let top = self.clamp_top(current + lines);
        let next = if top >= self.visible.start {
            None
        } else {
            Some(top)
        };
        if next == self.scroll_top {
            return false;
        }
        self.scroll_top = next;
        self.render_revision += 1;
        true
    }

    /// Back to the live screen.
    pub fn scroll_to_bottom(&mut self) {
        if self.scroll_top.take().is_some() {
            self.render_revision += 1;
        }
    }

    /// The gaps in `range` that are not held and not evicted: what to fetch.
    pub fn missing_rows(&self, range: StableRange) -> Vec<StableRange> {
        let mut out = Vec::new();
        let mut cursor = range.start.max(self.first_retained);
        let end = range.end;
        let mut held = self.rows.range(cursor..end).map(|(k, _)| *k).peekable();
        while cursor < end {
            match held.peek() {
                Some(&next) if next == cursor => {
                    held.next();
                    cursor += 1;
                }
                Some(&next) => {
                    out.push(StableRange {
                        start: cursor,
                        end: next,
                    });
                    cursor = next;
                }
                None => {
                    out.push(StableRange { start: cursor, end });
                    break;
                }
            }
        }
        out
    }

    /// Rows changed since the last call, for a renderer replacing arenas.
    pub fn take_dirty(&mut self) -> BTreeSet<i64> {
        std::mem::take(&mut self.dirty)
    }

    /// Events in order, drained.
    pub fn take_events(&mut self) -> Vec<TerminalEvent> {
        self.events.drain(..).collect()
    }

    /// Whether the cursor may be drawn from this state: only when the
    /// runner has applied at least this client's `latest_serial` input, so a
    /// position from before a keystroke does not flash back.
    pub fn cursor_is_current(&self, latest_serial: u64) -> bool {
        self.input_serial_ack >= latest_serial
    }

    pub fn is_blinking_cursor(&self) -> bool {
        self.cursor.visible
            && self.cursor.blinking
            && matches!(
                self.cursor.shape,
                CursorShape::Block | CursorShape::Underline | CursorShape::Bar
            )
    }

    fn clamp_scroll(&mut self) -> bool {
        let Some(top) = self.scroll_top else {
            return false;
        };
        let clamped = self.clamp_top(top);
        let next = if clamped >= self.visible.start {
            None
        } else {
            Some(clamped)
        };
        if next != self.scroll_top {
            self.scroll_top = next;
            true
        } else {
            false
        }
    }

    /// `top` bounded to where a scrolled viewport may start: no earlier than
    /// the oldest retained row, no later than the screen. The bounds come
    /// from different messages -- a page can report an eviction that already
    /// passed the screen the last delta named -- so the lower may exceed the
    /// upper for a moment; the screen wins, which follows it.
    fn clamp_top(&self, top: i64) -> i64 {
        top.max(self.first_retained).min(self.visible.start)
    }

    /// Drops rows farthest from the viewport until `capacity` holds.
    fn trim(&mut self) {
        while self.rows.len() > self.capacity {
            let viewport = self.viewport();
            let center = (viewport.start + viewport.end) / 2;
            let (&first, _) = self.rows.first_key_value().expect("non-empty");
            let (&last, _) = self.rows.last_key_value().expect("non-empty");
            let victim = if (center - first).abs() >= (last - center).abs() {
                first
            } else {
                last
            };
            self.rows.remove(&victim);
            self.dirty.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{CellSpan, CellStyle, Cursor, CursorShape};

    fn row(stable_row: i64, seq: u64, text: &str) -> RowData {
        RowData {
            stable_row,
            row_seq: seq,
            wrapped: false,
            spans: vec![CellSpan {
                start_col: 0,
                cell_count: text.len() as u16,
                text: text.into(),
                style: CellStyle::default(),
                link: None,
            }],
        }
    }

    fn head(rows: Vec<RowData>) -> TerminalHead {
        TerminalHead {
            terminal: TerminalId(1),
            epoch: 1,
            seq: 10,
            dimensions: Dimensions { cols: 80, rows: 2 },
            visible: StableRange { start: 8, end: 10 },
            first_retained: 0,
            cursor: Cursor {
                x: 0,
                y: 0,
                shape: CursorShape::Block,
                visible: true,
                blinking: false,
            },
            title: String::new(),
            modes: Modes::default(),
            palette: Palette::default(),
            rows,
            exit: None,
            controller: None,
            input_serial_ack: 0,
        }
    }

    fn delta(from: u64, to: u64) -> TerminalDelta {
        TerminalDelta {
            terminal: TerminalId(1),
            epoch: 1,
            from_seq: from,
            to_seq: to,
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
        }
    }

    #[test]
    fn a_delta_is_applied_only_at_its_base() {
        let mut view = TerminalView::from_head(head(vec![row(8, 9, "a"), row(9, 9, "b")]), 100);
        assert_eq!(
            view.apply_delta(delta(11, 12)),
            Err(Rejected::Gap {
                applied: 10,
                from: 11
            })
        );
        let mut d = delta(10, 11);
        d.row_replacements = vec![row(9, 11, "c")];
        assert_eq!(view.apply_delta(d), Ok(()));
        assert_eq!(view.applied_seq, 11);
        assert_eq!(view.row(9).unwrap().spans[0].text, "c");
        assert_eq!(
            view.take_dirty().into_iter().collect::<Vec<_>>(),
            vec![8, 9]
        );
        let mut other_epoch = delta(11, 12);
        other_epoch.epoch = 2;
        assert!(matches!(
            view.apply_delta(other_epoch),
            Err(Rejected::Epoch { .. })
        ));
    }

    #[test]
    fn eviction_drops_rows_and_clamps_the_scroll() {
        let mut view = TerminalView::from_head(
            head(vec![
                row(5, 1, "x"),
                row(6, 1, "y"),
                row(8, 9, "a"),
                row(9, 9, "b"),
            ]),
            100,
        );
        assert!(view.scroll_by(-10), "scrolls up to the oldest retained row");
        assert_eq!(view.viewport(), StableRange { start: 0, end: 2 });
        let mut d = delta(10, 11);
        d.evicted_before = Some(7);
        view.apply_delta(d).unwrap();
        assert!(view.row(5).is_none() && view.row(6).is_none());
        assert_eq!(view.first_retained, 7);
        assert_eq!(
            view.viewport().start,
            7,
            "the scroll cannot name evicted rows"
        );
        view.scroll_by(100);
        assert!(view.follows_screen());
    }

    #[test]
    fn an_older_page_never_overwrites_a_newer_row() {
        let mut view = TerminalView::from_head(head(vec![row(9, 12, "new")]), 100);
        let page = RowPage {
            terminal: TerminalId(1),
            epoch: 1,
            generation: 1,
            first_retained: 3,
            seq: 10,
            rows: vec![row(9, 10, "old"), row(4, 10, "four"), row(2, 10, "evicted")],
        };
        assert!(view.apply_page(page));
        assert_eq!(view.row(9).unwrap().spans[0].text, "new");
        assert!(view.row(4).is_some());
        assert!(view.row(2).is_none(), "below first_retained is dropped");
        assert_eq!(view.first_retained, 3);
    }

    #[test]
    fn an_eviction_past_the_screen_follows_the_screen() {
        let mut view = TerminalView::from_head(
            head(vec![row(4, 1, "d"), row(8, 9, "a"), row(9, 9, "b")]),
            100,
        );
        view.scroll_by(-4);
        assert_eq!(view.scroll_top, Some(4));
        // A burst evicted beyond the screen the last delta named; the page
        // saying so arrives before the delta that moves the screen.
        let page = RowPage {
            terminal: TerminalId(1),
            epoch: 1,
            generation: 1,
            first_retained: 20,
            seq: 30,
            rows: vec![],
        };
        view.apply_page(page);
        // The delta after it still names the old screen.
        let mut d = delta(10, 11);
        d.evicted_before = Some(20);
        view.apply_delta(d).unwrap();
        assert!(view.follows_screen());
        assert!(!view.scroll_by(-1), "nothing retained above the screen");
    }

    #[test]
    fn missing_rows_are_the_holes() {
        let view = TerminalView::from_head(
            head(vec![row(8, 9, "a"), row(9, 9, "b"), row(4, 1, "d")]),
            100,
        );
        assert_eq!(
            view.missing_rows(StableRange { start: 2, end: 10 }),
            vec![
                StableRange { start: 2, end: 4 },
                StableRange { start: 5, end: 8 }
            ]
        );
        assert!(
            view.missing_rows(StableRange { start: 8, end: 10 })
                .is_empty()
        );
    }

    #[test]
    fn the_store_is_bounded_around_the_viewport() {
        let rows: Vec<_> = (0..50).map(|i| row(i, 1, "r")).collect();
        let mut h = head(rows);
        h.visible = StableRange { start: 48, end: 50 };
        let view = TerminalView::from_head(h, 10);
        assert_eq!(view.rows_in(StableRange { start: 0, end: 50 }).count(), 10);
        assert!(view.row(49).is_some(), "the screen is kept");
        assert!(view.row(0).is_none(), "the far history goes first");
    }

    #[test]
    fn a_head_under_the_user_keeps_their_scroll() {
        let mut view = TerminalView::from_head(
            head(vec![row(4, 1, "d"), row(8, 9, "a"), row(9, 9, "b")]),
            100,
        );
        view.scroll_by(-4);
        assert_eq!(view.scroll_top, Some(4));
        let mut fresh = head(vec![row(8, 20, "a"), row(9, 20, "b")]);
        fresh.seq = 20;
        fresh.first_retained = 6;
        view.apply_head(fresh);
        assert_eq!(view.scroll_top, Some(6), "clamped to what still exists");
        assert_eq!(view.applied_seq, 20);
    }
}
