//! What the host's theme decides about a terminal surface.
//!
//! The child owns its colours: everything it set through `OSC 4`/`10`/`11`
//! travels in the view's own palette and is drawn as it asked. A theme fills
//! only the slots the child never touched -- [`zeughaus_mux::Palette::themed`]
//! is where that merge happens -- so a `Style` is a *fallback* palette plus
//! the one colour no terminal protocol names: the selection highlight, which
//! belongs to the client because the selection itself does.

use iced::Color;

use zeughaus_mux::Palette;

/// The appearance of one terminal surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// The colours a terminal that changed nothing is drawn with.
    pub palette: Palette,
    /// Filled behind the cells the user selected, drawn under the glyphs.
    pub selection: Color,
}

/// A function returning the style of a terminal for some theme.
pub type StyleFn<'a, Theme> = Box<dyn Fn(&Theme) -> Style + 'a>;

/// The theme catalog of a terminal.
pub trait Catalog {
    /// The style class this theme addresses a terminal with.
    type Class<'a>;

    /// The default class.
    fn default<'a>() -> Self::Class<'a>;

    /// The style of a terminal in this class.
    fn style(&self, class: &Self::Class<'_>) -> Style;
}

impl Catalog for iced::Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(default)
    }

    fn style(&self, class: &Self::Class<'_>) -> Style {
        class(self)
    }
}

/// The default terminal style: the colours the runner's terminal core starts
/// with, so an unthemed host looks exactly like the child expects, and the
/// theme's own weak accent for the selection.
pub fn default(theme: &iced::Theme) -> Style {
    Style {
        palette: Palette::RUNNER_DEFAULT,
        selection: theme.extended_palette().primary.weak.color,
    }
}
