//! The grid: how many cells fit into a rectangle, and where a point lands.
//!
//! Everything here is logical pixels, the unit iced hands a widget. A cell is
//! drawn a whole number of device pixels wide and high -- a cell that is 8.4 px
//! wide logically would otherwise put its columns at fractional positions --
//! so the grid is counted with [`CellMetrics::snapped`] at the same scale the
//! pipeline draws with; counted with the unrounded cell, the last columns of
//! a wide pane would be drawn past its edge.
//!
//! Column and row counts are always floored and never zero: a pane one pixel
//! high still has a 1x1 grid, because a [`Dimensions`] of zero is not a size a
//! PTY accepts.

use iced::{Point, Size};
use zeughaus_mux::Dimensions;
use zeughaus_mux::terminal::{MAX_COLS, MAX_ROWS};

/// What one cell of the bundled font measures at one font size.
///
/// `width` is the advance of a monospace glyph, `height` the distance from one
/// baseline to the next. The decoration offsets are measured down from the
/// cell's top edge, so a renderer never has to know where the baseline is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    /// Baseline, measured down from the top of the cell.
    pub ascent: f32,
    pub descent: f32,
    pub underline_offset: f32,
    pub underline_thickness: f32,
    pub strikethrough_offset: f32,
}

impl CellMetrics {
    /// The cell as a size, which is all the layout needs.
    pub fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    /// The cell as the pipeline draws it at `scale` device pixels per logical
    /// pixel: width and height rounded to whole device pixels, expressed in
    /// logical pixels again. The pipeline rounds the same way, so a grid
    /// counted with this cell ends where the drawn one does.
    pub fn snapped(self, scale: f32) -> Self {
        if !scale.is_finite() || scale <= 0.0 {
            return self;
        }
        let snap = |length: f32| (length * scale).round().max(1.0) / scale;
        CellMetrics {
            width: snap(self.width),
            height: snap(self.height),
            ..self
        }
    }
}

/// How many whole cells fit into `bounds`.
///
/// Floored, clamped into what the wire accepts, and never zero -- a partially
/// visible last row is not a row the child may write to.
pub fn grid_size(bounds: Size, cell: CellMetrics) -> Dimensions {
    Dimensions {
        cols: count(bounds.width, cell.width, MAX_COLS),
        rows: count(bounds.height, cell.height, MAX_ROWS),
    }
}

fn count(available: f32, per_cell: f32, max: u16) -> u16 {
    if !available.is_finite() || !per_cell.is_finite() || per_cell <= 0.0 || available <= 0.0 {
        return 1;
    }
    let fit = (available / per_cell).floor();
    if !fit.is_finite() || fit < 1.0 {
        return 1;
    }
    (fit as u32).min(u32::from(max)) as u16
}

/// The cell a point inside the widget lands on, clamped to the grid.
///
/// `local` is relative to the widget's top-left corner. Clamping rather than
/// rejecting is deliberate: a drag that leaves the pane keeps selecting at the
/// edge it left through, which is what every terminal does.
pub fn cell_at(local: Point, cell: CellMetrics, grid: Dimensions) -> (u16, u16) {
    let col = axis(local.x, cell.width, grid.cols);
    let row = axis(local.y, cell.height, grid.rows);
    (col, row)
}

fn axis(offset: f32, per_cell: f32, count: u16) -> u16 {
    if !offset.is_finite() || per_cell <= 0.0 || offset <= 0.0 {
        return 0;
    }
    let index = (offset / per_cell).floor();
    if !index.is_finite() || index < 0.0 {
        return 0;
    }
    (index as u32).min(u32::from(count.saturating_sub(1))) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellMetrics = CellMetrics {
        width: 10.0,
        height: 20.0,
        ascent: 15.0,
        descent: 5.0,
        underline_offset: 17.0,
        underline_thickness: 1.0,
        strikethrough_offset: 10.0,
    };

    #[test]
    fn grid_floors_and_never_reaches_zero() {
        assert_eq!(
            grid_size(Size::new(105.0, 59.0), CELL),
            Dimensions { cols: 10, rows: 2 }
        );
        // Less than one cell in either direction is still a 1x1 terminal.
        assert_eq!(
            grid_size(Size::new(3.0, 1.0), CELL),
            Dimensions { cols: 1, rows: 1 }
        );
        assert_eq!(
            grid_size(Size::new(0.0, 0.0), CELL),
            Dimensions { cols: 1, rows: 1 }
        );
        assert_eq!(
            grid_size(Size::new(f32::NAN, f32::INFINITY), CELL),
            Dimensions { cols: 1, rows: 1 }
        );
    }

    #[test]
    fn grid_stays_within_the_wire_bounds() {
        let huge = grid_size(Size::new(1_000_000.0, 1_000_000.0), CELL);
        assert_eq!(huge.cols, MAX_COLS);
        assert_eq!(huge.rows, MAX_ROWS);
        assert!(huge.is_valid());
    }

    #[test]
    fn a_snapped_grid_fits_the_pane_as_drawn() {
        // 8.8 px wide: at scale 2 a column is drawn 18 device pixels wide,
        // which counted as 8.8 logical pixels overran a 1000 px pane by 22.
        let cell = CellMetrics {
            width: 8.8,
            height: 19.2,
            ..CELL
        };
        let pane = Size::new(1000.0, 700.0);
        for scale in [1.0, 1.25, 1.5, 2.0, 2.5, 3.0] {
            let grid = grid_size(pane, cell.snapped(scale));
            let drawn_width = f32::from(grid.cols) * (cell.width * scale).round();
            let drawn_height = f32::from(grid.rows) * (cell.height * scale).round();
            assert!(drawn_width <= pane.width * scale, "{scale}: {grid:?}");
            assert!(drawn_height <= pane.height * scale, "{scale}: {grid:?}");
            // And no whole drawn column is left unused.
            assert!(drawn_width + (cell.width * scale).round() > pane.width * scale);
        }
    }

    #[test]
    fn a_nonsense_scale_leaves_the_cell_alone() {
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(CELL.snapped(scale), CELL);
        }
    }

    #[test]
    fn a_point_outside_the_grid_clamps_to_its_edge() {
        let grid = Dimensions { cols: 10, rows: 3 };
        assert_eq!(cell_at(Point::new(0.0, 0.0), CELL, grid), (0, 0));
        assert_eq!(cell_at(Point::new(25.0, 45.0), CELL, grid), (2, 2));
        assert_eq!(cell_at(Point::new(9999.0, 9999.0), CELL, grid), (9, 2));
        assert_eq!(cell_at(Point::new(-5.0, -5.0), CELL, grid), (0, 0));
    }
}
