//! The one face this widget draws with, and what a cell measures in it.
//!
//! A terminal grid needs a face whose glyphs all advance the same amount, so
//! the font is not a setting: ComicShannsMono Nerd Font Mono is bundled, in
//! regular and bold, and its icon glyphs come with it (licenses in `fonts/`).
//! The bytes are registered with iced's process-wide `FontSystem` the first
//! time anything here needs them, so measuring works before the application
//! got around to calling `iced::application(..).font(..)` -- and registering
//! twice is free, because iced deduplicates static byte slices by address.
//!
//! Cell metrics come from the face itself rather than a guessed line-height
//! factor: the advance width of a shaped glyph and the face's own
//! ascent/descent/leading. They are cached per font size because measuring
//! means shaping, and every frame asks for them.

use std::borrow::Cow;
use std::sync::{LazyLock, Mutex};

use iced::advanced::graphics::text::{cosmic_text, font_system};
use iced::font::{Family, Font, Stretch, Style, Weight};

use crate::geometry::CellMetrics;

/// The family name both faces report; shaping selects between them by weight.
pub const FAMILY: &str = "ComicShannsMono Nerd Font Mono";

const REGULAR_BYTES: &[u8] = include_bytes!("../fonts/ComicShannsMonoNerdFontMono-Regular.otf");
const BOLD_BYTES: &[u8] = include_bytes!("../fonts/ComicShannsMonoNerdFontMono-Bold.otf");

/// The bundled face, regular weight.
pub const FONT: Font = Font {
    family: Family::Name(FAMILY),
    weight: Weight::Normal,
    stretch: Stretch::Normal,
    style: Style::Normal,
};

/// The bundled face, bold weight: what a `BOLD` cell style draws with.
pub const FONT_BOLD: Font = Font {
    family: Family::Name(FAMILY),
    weight: Weight::Bold,
    stretch: Stretch::Normal,
    style: Style::Normal,
};

/// The font files to hand `iced::application(..).font(..)`.
///
/// Calling it is optional -- the crate registers the same bytes itself -- but
/// an application that loads its fonts up front avoids the first measurement
/// paying for it.
pub fn font_bytes() -> impl Iterator<Item = &'static [u8]> {
    [REGULAR_BYTES, BOLD_BYTES].into_iter()
}

/// The width and height of one cell at `font_size`, in logical pixels.
pub fn cell_geometry(font_size: f32) -> (f32, f32) {
    let metrics = cell_metrics(font_size);
    (metrics.width, metrics.height)
}

/// Full cell metrics at `font_size`, measured once per size.
pub fn cell_metrics(font_size: f32) -> CellMetrics {
    let size = sane_size(font_size);
    let key = size.to_bits();

    let cache = &METRIC_CACHE;
    if let Some(hit) = cache.lock().ok().and_then(|entries| {
        entries
            .iter()
            .find(|(bits, _)| *bits == key)
            .map(|(_, m)| *m)
    }) {
        return hit;
    }

    let measured = measure(size);
    if let Ok(mut entries) = cache.lock() {
        // A handful of sizes is all a session uses; the bound only stops a
        // pathological font-size animation from growing the table forever.
        if entries.len() >= 32 {
            entries.clear();
        }
        entries.push((key, measured));
    }
    measured
}

/// The metrics measured so far, keyed by the font size's bit pattern.
static METRIC_CACHE: LazyLock<Mutex<Vec<(u32, CellMetrics)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Registering the bundled faces with iced's font system, once per process.
static REGISTERED: LazyLock<()> = LazyLock::new(|| {
    if let Ok(mut fonts) = font_system().write() {
        fonts.load_font(Cow::Borrowed(REGULAR_BYTES));
        fonts.load_font(Cow::Borrowed(BOLD_BYTES));
    }
});

/// Makes sure iced's font system knows the bundled faces. Idempotent.
pub(crate) fn ensure_registered() {
    LazyLock::force(&REGISTERED);
}

/// Font sizes arrive from user settings and window scaling; clamp before they
/// reach the shaper, where a zero or a NaN is a panic or a division by zero.
fn sane_size(font_size: f32) -> f32 {
    if font_size.is_finite() {
        font_size.clamp(4.0, 256.0)
    } else {
        14.0
    }
}

fn measure(font_size: f32) -> CellMetrics {
    ensure_registered();

    let Ok(mut fonts) = font_system().write() else {
        return fallback(font_size);
    };
    let fonts = fonts.raw();

    let attrs = cosmic_text::Attrs::new()
        .family(cosmic_text::Family::Name(FAMILY))
        .weight(cosmic_text::Weight::NORMAL);

    // A nominal line height; the real one is computed from the face below.
    let mut buffer =
        cosmic_text::Buffer::new(fonts, cosmic_text::Metrics::new(font_size, font_size * 1.5));
    buffer.set_wrap(fonts, cosmic_text::Wrap::None);
    buffer.set_size(fonts, None, None);
    buffer.set_text(fonts, "M", &attrs, cosmic_text::Shaping::Advanced, None);
    buffer.shape_until_scroll(fonts, false);

    let glyph = buffer
        .layout_runs()
        .flat_map(|run| run.glyphs.iter())
        .next()
        .map(|glyph| (glyph.w, glyph.font_id, glyph.font_weight));

    let Some((advance, font_id, weight)) = glyph else {
        return fallback(font_size);
    };
    let Some(face) = fonts.get_font(font_id, weight) else {
        return fallback(font_size);
    };

    let face_metrics = face.metrics();
    let upem = f32::from(face_metrics.units_per_em.max(1));
    let scale = font_size / upem;

    let ascent = face_metrics.ascent * scale;
    let descent = face_metrics.descent.abs() * scale;
    let leading = face_metrics.leading.max(0.0) * scale;
    let height = (ascent + descent + leading).max(1.0);

    let underline_thickness = face_metrics
        .underline
        .map_or(font_size / 14.0, |line| line.thickness * scale)
        .max(1.0);
    // Decoration offsets are measured up from the baseline; a renderer wants
    // them down from the top of the cell.
    let underline_offset = face_metrics
        .underline
        .map_or(ascent + descent * 0.4, |line| ascent - line.offset * scale)
        .clamp(0.0, height - underline_thickness);
    let strikethrough_offset = face_metrics
        .strikeout
        .map_or(ascent * 0.6, |line| ascent - line.offset * scale)
        .clamp(0.0, height - underline_thickness);

    let width = if advance.is_finite() && advance > 0.0 {
        advance
    } else {
        font_size * 0.6
    };

    CellMetrics {
        width,
        height,
        ascent,
        descent,
        underline_offset,
        underline_thickness,
        strikethrough_offset,
    }
}

/// Used when the font system is unavailable (a poisoned lock) or the face did
/// not load: proportions close enough to keep a pane usable rather than empty.
fn fallback(font_size: f32) -> CellMetrics {
    let height = (font_size * 1.3).max(1.0);
    let thickness = (font_size / 14.0).max(1.0);
    CellMetrics {
        width: font_size * 0.6,
        height,
        ascent: height * 0.8,
        descent: height * 0.2,
        underline_offset: height * 0.9,
        underline_thickness: thickness,
        strikethrough_offset: height * 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_is_positive_and_grows_with_the_font_size() {
        let mut previous = (0.0f32, 0.0f32);
        for size in [8.0f32, 10.0, 12.0, 14.0, 18.0, 24.0, 36.0] {
            let (width, height) = cell_geometry(size);
            assert!(width > 0.0, "cell width at {size} must be positive");
            assert!(height > 0.0, "cell height at {size} must be positive");
            assert!(
                width > previous.0 && height > previous.1,
                "cell geometry must grow with the font size: {size} gave {width}x{height} after {previous:?}"
            );
            previous = (width, height);
        }
    }

    #[test]
    fn metrics_are_consistent_within_a_cell() {
        let metrics = cell_metrics(14.0);
        assert!(metrics.ascent > 0.0 && metrics.descent >= 0.0);
        assert!(metrics.ascent + metrics.descent <= metrics.height + 0.001);
        assert!(metrics.underline_thickness >= 1.0);
        assert!(metrics.underline_offset >= 0.0 && metrics.underline_offset < metrics.height);
        assert!(
            metrics.strikethrough_offset >= 0.0 && metrics.strikethrough_offset < metrics.height
        );
    }

    #[test]
    fn a_nonsense_font_size_still_measures() {
        let (width, height) = cell_geometry(f32::NAN);
        assert!(width > 0.0 && height > 0.0);
        let (width, height) = cell_geometry(-3.0);
        assert!(width > 0.0 && height > 0.0);
    }
}
