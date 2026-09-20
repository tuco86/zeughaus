//! What the renderer keeps between frames: shaped rows, built instances, and
//! the glyph atlas's free space.
//!
//! The decomposition is WezTerm's, because it is the one that makes a
//! character-by-character terminal cheap. A row is shaped once per distinct
//! *content*, not per frame and not per screen position: the shape cache is
//! keyed by a hash of the row's spans plus the font size and the scale factor,
//! the only three things that can change where a glyph lands. The instance
//! cache sits above it and is keyed by the same hash plus the palette and the
//! atlas generation -- a palette change recolours without reshaping, and an
//! atlas rebuild invalidates the texture coordinates but not the shaping.
//!
//! Both caches are bounded. So is the atlas: a shelf packer over a fixed
//! square, and when it runs out the whole atlas is thrown away and rebuilt
//! from the rows still on screen. That is a visible hitch once in a very long
//! while, and it is preferable to an allocator that can grow without limit.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use unicode_width::UnicodeWidthChar;
use zeughaus_mux::{CellSpan, Palette};

/// Identifies a row's *appearance*: everything that decides which glyph lands
/// in which cell. Deliberately not the stable row index or the row sequence --
/// two rows with the same text shape identically and share one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RowKey {
    pub content: u64,
    pub font_size: u32,
    /// The device scale factor. The widget does not know it -- only the
    /// renderer's viewport does -- so a key is stamped with it there.
    pub scale: u32,
}

impl RowKey {
    pub fn with_scale(self, scale: f32) -> RowKey {
        RowKey {
            scale: scale.to_bits(),
            ..self
        }
    }
}

/// The key of a built row of GPU instances: a shaped row resolved against one
/// palette, with texture coordinates from one atlas generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct InstanceKey {
    pub row: RowKey,
    pub palette: u64,
    pub atlas: u64,
    /// Reverse video swaps every cell's foreground and background.
    pub reverse_video: bool,
}

/// Hashes a row's spans into a [`RowKey`], without a scale factor yet.
pub(crate) fn row_key(spans: &[CellSpan], font_size: f32) -> RowKey {
    let mut hasher = DefaultHasher::new();
    spans.len().hash(&mut hasher);
    for span in spans {
        span.start_col.hash(&mut hasher);
        span.cell_count.hash(&mut hasher);
        span.text.hash(&mut hasher);
        span.style.hash(&mut hasher);
        span.link.hash(&mut hasher);
    }
    RowKey {
        content: hasher.finish(),
        font_size: font_size.to_bits(),
        scale: 0,
    }
}

/// Hashes a palette so a change to it can invalidate built instances without
/// comparing 19 colours per row.
pub(crate) fn palette_generation(palette: &Palette) -> u64 {
    let mut hasher = DefaultHasher::new();
    palette.ansi.hash(&mut hasher);
    palette.foreground.hash(&mut hasher);
    palette.background.hash(&mut hasher);
    palette.cursor.hash(&mut hasher);
    hasher.finish()
}

/// The grid column of every character of a span.
///
/// A combining mark has zero width and belongs to the cell of the character it
/// follows; a wide glyph advances two. The runner counted cells the same way
/// when it built `cell_count`, so a row laid out from this lands where the
/// child expects it.
pub(crate) fn char_columns(span: &CellSpan) -> CharColumns<'_> {
    CharColumns {
        chars: span.text.char_indices(),
        col: span.start_col,
        base: span.start_col,
    }
}

pub(crate) struct CharColumns<'a> {
    chars: std::str::CharIndices<'a>,
    col: u16,
    base: u16,
}

impl Iterator for CharColumns<'_> {
    /// `(byte offset in the span's text, character, grid column)`
    type Item = (usize, char, u16);

    fn next(&mut self) -> Option<Self::Item> {
        let (offset, ch) = self.chars.next()?;
        let width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if width == 0 {
            return Some((offset, ch, self.base));
        }
        self.base = self.col;
        let col = self.col;
        self.col = self.col.saturating_add(width);
        Some((offset, ch, col))
    }
}

/// A bounded cache that drops the least recently used quarter when it is full.
///
/// Not a linked-list LRU: a terminal touches every live entry once per frame,
/// so the access order within a frame carries no information and the only
/// thing that matters is that entries nobody drew for a while eventually go.
#[derive(Debug)]
pub(crate) struct Lru<K, V> {
    entries: HashMap<K, (V, u64)>,
    clock: u64,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    pub fn new(capacity: usize) -> Self {
        Lru {
            entries: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        entry.1 = clock;
        Some(&entry.0)
    }

    pub fn insert(&mut self, key: K, value: V) {
        self.clock += 1;
        let _ = self.entries.insert(key, (value, self.clock));
        if self.entries.len() > self.capacity {
            self.evict();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn evict(&mut self) {
        let keep = self.capacity - self.capacity / 4;
        let mut stamps: Vec<u64> = self.entries.values().map(|(_, stamp)| *stamp).collect();
        stamps.sort_unstable();
        let Some(threshold) = stamps.get(stamps.len().saturating_sub(keep)).copied() else {
            return;
        };
        self.entries.retain(|_, (_, stamp)| *stamp >= threshold);
    }
}

/// A shelf packer over a fixed square atlas.
///
/// Glyph heights at one font size fall into a handful of buckets, so shelves
/// fill densely without a general rectangle packer. Heights are rounded up to
/// a bucket so that a one-pixel difference does not open a new shelf.
#[derive(Debug)]
pub(crate) struct Shelf {
    size: u32,
    rows: Vec<ShelfRow>,
    next_y: u32,
}

#[derive(Debug)]
struct ShelfRow {
    y: u32,
    height: u32,
    next_x: u32,
}

/// Heights are rounded up to a multiple of this before a shelf is chosen.
const HEIGHT_BUCKET: u32 = 4;

impl Shelf {
    pub fn new(size: u32) -> Self {
        Shelf {
            size: size.max(1),
            rows: Vec::new(),
            next_y: 0,
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// Reserves a `width` x `height` rectangle, or `None` when the atlas is
    /// full. A zero-sized request allocates nothing and reports the origin.
    pub fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width == 0 || height == 0 {
            return Some((0, 0));
        }
        if width > self.size || height > self.size {
            return None;
        }
        let bucket = height.div_ceil(HEIGHT_BUCKET) * HEIGHT_BUCKET;

        for row in &mut self.rows {
            if row.height >= bucket && row.next_x + width <= self.size {
                let origin = (row.next_x, row.y);
                row.next_x += width;
                return Some(origin);
            }
        }

        if self.next_y + bucket > self.size {
            return None;
        }
        let y = self.next_y;
        self.next_y += bucket;
        self.rows.push(ShelfRow {
            y,
            height: bucket,
            next_x: width,
        });
        Some((0, y))
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.next_y = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_mux::{CellStyle, StyleFlags, Underline, WireColor};

    fn span(start_col: u16, text: &str, style: CellStyle) -> CellSpan {
        CellSpan {
            start_col,
            cell_count: text.chars().count() as u16,
            text: text.to_string(),
            style,
            link: None,
        }
    }

    #[test]
    fn a_row_key_tracks_content_style_size_and_scale() {
        let plain = vec![span(0, "hello", CellStyle::default())];
        let base = row_key(&plain, 14.0).with_scale(1.0);
        let keyed = |spans: &[CellSpan]| row_key(spans, 14.0).with_scale(1.0);

        assert_eq!(base, keyed(&plain), "same input, same key");

        assert_ne!(base, keyed(&[span(0, "hellp", CellStyle::default())]));
        assert_ne!(base, keyed(&[span(1, "hello", CellStyle::default())]));

        let bold = CellStyle {
            flags: StyleFlags::default().with(StyleFlags::BOLD),
            ..CellStyle::default()
        };
        assert_ne!(base, keyed(&[span(0, "hello", bold)]));

        let underlined = CellStyle {
            flags: StyleFlags::default().with_underline(Underline::Curly),
            ..CellStyle::default()
        };
        assert_ne!(base, keyed(&[span(0, "hello", underlined)]));

        let coloured = CellStyle {
            fg: WireColor::Indexed(4),
            ..CellStyle::default()
        };
        assert_ne!(base, keyed(&[span(0, "hello", coloured)]));

        assert_ne!(
            base,
            row_key(&plain, 14.5).with_scale(1.0),
            "font size is part of it"
        );
        assert_ne!(
            base,
            row_key(&plain, 14.0).with_scale(2.0),
            "so is the scale factor"
        );
    }

    #[test]
    fn a_palette_generation_follows_the_palette() {
        let palette = Palette::default();
        let mut changed = Palette::default();
        changed.ansi[3] = [1, 2, 3];
        assert_eq!(
            palette_generation(&palette),
            palette_generation(&Palette::default())
        );
        assert_ne!(palette_generation(&palette), palette_generation(&changed));
    }

    #[test]
    fn combining_marks_share_a_cell_and_wide_glyphs_take_two() {
        let span = span(2, "a\u{0301}\u{4e16}b", CellStyle::default());
        let columns: Vec<(char, u16)> = char_columns(&span).map(|(_, ch, col)| (ch, col)).collect();
        assert_eq!(
            columns,
            vec![('a', 2), ('\u{0301}', 2), ('\u{4e16}', 3), ('b', 5)]
        );
    }

    #[test]
    fn the_shelf_stays_inside_the_atlas_and_reports_when_it_is_full() {
        let mut shelf = Shelf::new(64);
        let mut placed = Vec::new();
        while let Some((x, y)) = shelf.allocate(9, 15) {
            assert!(
                x + 9 <= 64 && y + 15 <= 64,
                "allocation {x},{y} left the atlas"
            );
            placed.push((x, y));
            assert!(placed.len() <= 64, "the atlas cannot hold this many");
        }
        assert!(!placed.is_empty());
        // 7 columns of 9 px per shelf, 4 shelves of 16 px in 64 px.
        assert_eq!(placed.len(), 28);

        // Nothing inside one shelf overlaps.
        let mut sorted = placed.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), placed.len());

        shelf.clear();
        assert_eq!(shelf.allocate(9, 15), Some((0, 0)));
        assert_eq!(shelf.allocate(65, 15), None, "wider than the atlas");
        assert_eq!(
            shelf.allocate(0, 0),
            Some((0, 0)),
            "an empty glyph costs nothing"
        );
    }

    #[test]
    fn the_lru_stays_within_its_capacity() {
        let mut lru: Lru<u32, u32> = Lru::new(8);
        for key in 0..64 {
            lru.insert(key, key);
            assert!(lru.len() <= 8);
        }
        // The most recent insert survives; something old did not.
        assert_eq!(lru.get(&63), Some(&63));
        assert_eq!(lru.get(&0), None);
    }

    #[test]
    fn the_lru_keeps_what_is_still_being_read() {
        let mut lru: Lru<u32, u32> = Lru::new(8);
        for key in 0..8 {
            lru.insert(key, key);
        }
        for _ in 0..4 {
            assert_eq!(lru.get(&0), Some(&0));
        }
        for key in 8..12 {
            lru.insert(key, key);
        }
        assert_eq!(
            lru.get(&0),
            Some(&0),
            "a row still on screen is not evicted"
        );
    }
}
