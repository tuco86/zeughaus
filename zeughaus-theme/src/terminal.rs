//! The terminal panes' catalog.
//!
//! What a theme hands a terminal is a *fallback* palette: the widget merges it
//! with the child's own through [`zeughaus_mux::Palette::themed`], so every
//! colour an `OSC 4`/`10`/`11` set stays what the child asked for and only the
//! untouched slots become the theme's. The selection is the one colour no
//! terminal protocol names, and it belongs to the client because the selection
//! does.

use iced::Color;
use iced_terminal::{Catalog, Style, StyleFn};
use zeughaus_mux::Palette;

use crate::Theme;

impl Theme {
    /// This theme's scheme as the palette a terminal states colours in.
    pub fn terminal_palette(&self) -> Palette {
        let scheme = self.scheme();
        let mut ansi = [[0u8; 3]; 16];

        for (slot, color) in ansi.iter_mut().zip(scheme.ansi) {
            *slot = rgb(color);
        }

        Palette {
            ansi,
            foreground: rgb(scheme.foreground),
            background: rgb(scheme.background),
            cursor: rgb(scheme.cursor),
        }
    }
}

fn rgb(color: Color) -> [u8; 3] {
    let [red, green, blue, _alpha] = color.into_rgba8();

    [red, green, blue]
}

impl Catalog for Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(|theme: &Self| Style {
            palette: theme.terminal_palette(),
            selection: theme.scheme().selection,
        })
    }

    fn style(&self, class: &Self::Class<'_>) -> Style {
        class(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_is_the_scheme() {
        let theme = &Theme::pack()[2];
        let palette = theme.terminal_palette();

        assert_eq!(palette.ansi[0], rgb(theme.scheme().ansi[0]));
        assert_eq!(palette.ansi[15], rgb(theme.scheme().ansi[15]));
        assert_eq!(palette.foreground, rgb(theme.scheme().foreground));
        assert_eq!(palette.background, rgb(theme.scheme().background));
        assert_eq!(palette.cursor, rgb(theme.scheme().cursor));

        // Dracula's background is #282a36: the round trip through `Color` must
        // not drift, or a themed terminal would not match the chrome around
        // it.
        assert_eq!(palette.background, [0x28, 0x2a, 0x36]);
    }
}
