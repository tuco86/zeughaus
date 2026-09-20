//! Selecting text with the mouse, and turning a selection back into a string.
//!
//! Selection is entirely client-side state: the runner never hears about it,
//! and a viewer without the control lease may select and copy freely. It is
//! expressed in stable-row/column space rather than screen coordinates, so
//! scrolling, new output and a resize do not move it under the user.
//!
//! A logical line the grid wrapped is several [`RowData`] rows with `wrapped`
//! set on all but the last. Copying joins those without a newline -- what the
//! shell printed was one line, and pasting it back has to be one line again.

use zeughaus_mux::RowData;
use zeughaus_mux::view::TerminalView;

use crate::cache::char_columns;

/// A cell, addressed the way the wire addresses rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GridPoint {
    pub row: i64,
    pub col: u16,
}

impl GridPoint {
    pub fn new(row: i64, col: u16) -> Self {
        GridPoint { row, col }
    }
}

/// What one drag selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Cell to cell, the plain drag.
    Char,
    /// Whole words, from a double click.
    Word,
    /// Whole logical lines, from a triple click.
    Line,
}

/// An in-progress or finished selection. `anchor` is where the drag started
/// and does not move; `head` follows the pointer, and may be before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: GridPoint,
    pub head: GridPoint,
    pub mode: Mode,
}

impl Selection {
    pub fn new(at: GridPoint, mode: Mode) -> Self {
        Selection {
            anchor: at,
            head: at,
            mode,
        }
    }

    /// The selection as an ordered half-open range of cells, with the mode
    /// applied: a word selection reaches the word boundaries, a line
    /// selection the whole logical line.
    pub fn resolve(&self, view: &TerminalView) -> (GridPoint, GridPoint) {
        let (mut start, mut end) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        // The cell under the pointer is part of the selection.
        end.col = end.col.saturating_add(1);

        match self.mode {
            Mode::Char => {}
            Mode::Word => {
                start.col = word_start(view, start.row, start.col);
                end.col = word_end(view, end.row, end.col.saturating_sub(1));
            }
            Mode::Line => {
                start.row = logical_line_start(view, start.row);
                start.col = 0;
                end.row = logical_line_end(view, end.row);
                end.col = u16::MAX;
            }
        }
        (start, end)
    }

    /// Whether the selection covers no cell at all.
    pub fn is_empty(&self, view: &TerminalView) -> bool {
        let (start, end) = self.resolve(view);
        start.row == end.row && start.col >= end.col
    }
}

/// The columns of `row` covered by an ordered selection range, if any.
pub fn row_span(range: (GridPoint, GridPoint), row: i64, cols: u16) -> Option<(u16, u16)> {
    let (start, end) = range;
    if row < start.row || row > end.row {
        return None;
    }
    let from = if row == start.row { start.col } else { 0 };
    let to = if row == end.row { end.col } else { cols };
    let to = to.min(cols);
    if from >= to { None } else { Some((from, to)) }
}

/// The selected text, ready for the clipboard.
pub fn extract(view: &TerminalView, selection: &Selection) -> String {
    let (start, end) = selection.resolve(view);
    let mut out = String::new();
    let mut row_index = start.row;
    while row_index <= end.row {
        let Some(row) = view.row(row_index) else {
            if row_index < end.row {
                out.push('\n');
            }
            row_index += 1;
            continue;
        };
        let from = if row_index == start.row { start.col } else { 0 };
        let to = if row_index == end.row {
            end.col
        } else {
            u16::MAX
        };
        let mut text = row_text_in(row, from, to);
        if !row.wrapped {
            // Cells a line never wrote are blanks the wire omits; a copy of
            // them is trailing whitespace nobody asked for.
            while text.ends_with(' ') {
                let _ = text.pop();
            }
        }
        out.push_str(&text);
        if row_index < end.row && !row.wrapped {
            out.push('\n');
        }
        row_index += 1;
    }
    out
}

/// The hyperlink a cell carries, if the runner attached one. Never followed
/// without an explicit user action.
pub fn link_at(view: &TerminalView, row: i64, col: u16) -> Option<&str> {
    let row = view.row(row)?;
    row.spans
        .iter()
        .find(|span| col >= span.start_col && col < span.start_col.saturating_add(span.cell_count))
        .and_then(|span| span.link.as_deref())
}

fn row_text_in(row: &RowData, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut next_col = from;
    for span in &row.spans {
        if span.start_col >= to {
            continue;
        }
        for (_, ch, col) in char_columns(span) {
            if col >= to {
                break;
            }
            let width = char_width(ch);
            if col < from {
                continue;
            }
            if width > 0 {
                for _ in next_col..col {
                    out.push(' ');
                }
                next_col = col.saturating_add(width);
            }
            out.push(ch);
        }
    }
    out
}

fn char_width(ch: char) -> u16 {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16
}

/// Every character of a row with the column it starts in, in column order.
fn row_chars(view: &TerminalView, row: i64) -> Vec<(u16, char)> {
    let Some(row) = view.row(row) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for span in &row.spans {
        for (_, ch, col) in char_columns(span) {
            if char_width(ch) > 0 {
                out.push((col, ch));
            }
        }
    }
    out.sort_unstable_by_key(|(col, _)| *col);
    out
}

/// What counts as one word to a double click. Path and URL punctuation is
/// included because selecting a path is the usual reason to double click.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '~' | ':' | '@' | '+' | '=')
}

fn word_start(view: &TerminalView, row: i64, col: u16) -> u16 {
    let chars = row_chars(view, row);
    let mut start = col;
    for (at, ch) in chars.iter().rev() {
        if *at >= col {
            continue;
        }
        if !is_word(*ch) {
            break;
        }
        start = *at;
    }
    start
}

fn word_end(view: &TerminalView, row: i64, col: u16) -> u16 {
    let chars = row_chars(view, row);
    let mut end = col.saturating_add(1);
    for (at, ch) in &chars {
        if *at < col {
            continue;
        }
        if !is_word(*ch) {
            break;
        }
        end = at.saturating_add(char_width(*ch).max(1));
    }
    end
}

/// Walks back over rows the grid wrapped to the first row of the logical line.
fn logical_line_start(view: &TerminalView, row: i64) -> i64 {
    let mut first = row;
    while view.row(first - 1).is_some_and(|above| above.wrapped) {
        first -= 1;
    }
    first
}

fn logical_line_end(view: &TerminalView, row: i64) -> i64 {
    let mut last = row;
    while view.row(last).is_some_and(|here| here.wrapped) {
        last += 1;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_mux::{
        CellSpan, CellStyle, Cursor, CursorShape, Dimensions, Modes, Palette, StableRange,
        TerminalHead, TerminalId,
    };

    fn span(start_col: u16, text: &str) -> CellSpan {
        CellSpan {
            start_col,
            cell_count: text.chars().count() as u16,
            text: text.to_string(),
            style: CellStyle::default(),
            link: None,
        }
    }

    fn row(stable_row: i64, wrapped: bool, spans: Vec<CellSpan>) -> RowData {
        RowData {
            stable_row,
            row_seq: 1,
            wrapped,
            spans,
        }
    }

    fn view(rows: Vec<RowData>) -> TerminalView {
        let head = TerminalHead {
            terminal: TerminalId(7),
            epoch: 1,
            seq: 1,
            dimensions: Dimensions { cols: 20, rows: 4 },
            visible: StableRange { start: 0, end: 4 },
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
        };
        TerminalView::from_head(head, 64)
    }

    #[test]
    fn a_wrapped_line_copies_as_one_line() {
        let view = view(vec![
            row(0, true, vec![span(0, "hello ")]),
            row(1, false, vec![span(0, "world")]),
            row(2, false, vec![span(0, "next")]),
        ]);
        let selection = Selection {
            anchor: GridPoint::new(0, 0),
            head: GridPoint::new(2, 3),
            mode: Mode::Char,
        };
        assert_eq!(extract(&view, &selection), "hello world\nnext");
    }

    #[test]
    fn a_partial_selection_takes_only_the_cells_between_the_points() {
        let view = view(vec![
            row(0, false, vec![span(0, "abcdef")]),
            row(1, false, vec![span(0, "ghijkl")]),
        ]);
        let selection = Selection {
            anchor: GridPoint::new(0, 2),
            head: GridPoint::new(1, 1),
            mode: Mode::Char,
        };
        assert_eq!(extract(&view, &selection), "cdef\ngh");
    }

    #[test]
    fn a_selection_is_the_same_dragged_backwards() {
        let view = view(vec![row(0, false, vec![span(0, "abcdef")])]);
        let forwards = Selection {
            anchor: GridPoint::new(0, 1),
            head: GridPoint::new(0, 3),
            mode: Mode::Char,
        };
        let backwards = Selection {
            anchor: GridPoint::new(0, 3),
            head: GridPoint::new(0, 1),
            mode: Mode::Char,
        };
        assert_eq!(extract(&view, &forwards), "bcd");
        assert_eq!(extract(&view, &backwards), "bcd");
    }

    #[test]
    fn gaps_between_spans_copy_as_spaces_and_the_tail_is_trimmed() {
        let view = view(vec![row(0, false, vec![span(0, "ab"), span(5, "cd")])]);
        let whole = Selection {
            anchor: GridPoint::new(0, 0),
            head: GridPoint::new(0, 19),
            mode: Mode::Char,
        };
        assert_eq!(extract(&view, &whole), "ab   cd");
    }

    #[test]
    fn a_double_click_takes_the_word_under_it() {
        let view = view(vec![row(0, false, vec![span(0, "cd /tmp/a.txt x")])]);
        let selection = Selection::new(GridPoint::new(0, 6), Mode::Word);
        assert_eq!(extract(&view, &selection), "/tmp/a.txt");
    }

    #[test]
    fn a_triple_click_takes_the_whole_wrapped_line() {
        let view = view(vec![
            row(0, false, vec![span(0, "before")]),
            row(1, true, vec![span(0, "one ")]),
            row(2, false, vec![span(0, "two")]),
        ]);
        let selection = Selection::new(GridPoint::new(2, 1), Mode::Line);
        assert_eq!(extract(&view, &selection), "one two");
    }

    #[test]
    fn a_row_span_is_clipped_to_the_grid() {
        let range = (GridPoint::new(0, 3), GridPoint::new(2, 4));
        assert_eq!(row_span(range, 0, 10), Some((3, 10)));
        assert_eq!(row_span(range, 1, 10), Some((0, 10)));
        assert_eq!(row_span(range, 2, 10), Some((0, 4)));
        assert_eq!(row_span(range, 3, 10), None);
        assert_eq!(
            row_span((GridPoint::new(0, 4), GridPoint::new(0, 4)), 0, 10),
            None
        );
    }

    #[test]
    fn a_link_is_reported_only_for_the_cells_that_carry_it() {
        let mut linked = span(4, "docs");
        linked.link = Some("https://example.invalid/".to_string());
        let view = view(vec![row(0, false, vec![span(0, "see "), linked])]);
        assert_eq!(link_at(&view, 0, 0), None);
        assert_eq!(link_at(&view, 0, 5), Some("https://example.invalid/"));
        assert_eq!(link_at(&view, 0, 9), None);
    }
}
