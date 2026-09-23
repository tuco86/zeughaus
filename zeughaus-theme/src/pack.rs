//! The built-in themes: every [`iced::Theme::ALL`] entry with the terminal
//! colour scheme of the same name.
//!
//! The schemes are compiled in, parsed once and kept for the process. They are
//! `schemes/<iced theme name>.toml`, verbatim from upstream, all MIT licensed
//! (`schemes/LICENSE`). Twenty come from `mbadolato/iTerm2-Color-Schemes`
//! (its `wezterm/` directory); the two iced themes that repository has no
//! scheme for come from the projects the iced themes themselves are ports of.
//!
//! | iced theme           | schemes/               | upstream file                         |
//! |----------------------|------------------------|---------------------------------------|
//! | Light                | Light.toml             | iTerm2: `Builtin Light.toml`          |
//! | Dark                 | Dark.toml              | iTerm2: `Builtin Dark.toml`           |
//! | Dracula              | Dracula.toml           | iTerm2: `Dracula.toml`                |
//! | Nord                 | Nord.toml              | iTerm2: `Nord.toml`                   |
//! | Solarized Light      | Solarized Light.toml   | iTerm2: `iTerm2 Solarized Light.toml` |
//! | Solarized Dark       | Solarized Dark.toml    | iTerm2: `iTerm2 Solarized Dark.toml`  |
//! | Gruvbox Light        | Gruvbox Light.toml     | iTerm2: `Gruvbox Light.toml`          |
//! | Gruvbox Dark         | Gruvbox Dark.toml      | iTerm2: `Gruvbox Dark.toml`           |
//! | Catppuccin Latte     | Catppuccin Latte.toml  | iTerm2: `Catppuccin Latte.toml`       |
//! | Catppuccin Frappé    | Catppuccin Frappe.toml | iTerm2: `Catppuccin Frappe.toml`      |
//! | Catppuccin Macchiato | Catppuccin Macchiato.toml | iTerm2: `Catppuccin Macchiato.toml` |
//! | Catppuccin Mocha     | Catppuccin Mocha.toml  | iTerm2: `Catppuccin Mocha.toml`       |
//! | Tokyo Night          | Tokyo Night.toml       | iTerm2: `TokyoNight.toml`             |
//! | Tokyo Night Storm    | Tokyo Night Storm.toml | iTerm2: `TokyoNight Storm.toml`       |
//! | Tokyo Night Light    | Tokyo Night Light.toml | iTerm2: `TokyoNight Day.toml`         |
//! | Kanagawa Wave        | Kanagawa Wave.toml     | iTerm2: `Kanagawa Wave.toml`          |
//! | Kanagawa Dragon      | Kanagawa Dragon.toml   | iTerm2: `Kanagawa Dragon.toml`        |
//! | Kanagawa Lotus       | Kanagawa Lotus.toml    | iTerm2: `Kanagawa Lotus.toml`         |
//! | Moonfly              | Moonfly.toml           | iTerm2: `Moonfly.toml`                |
//! | Nightfly             | Nightfly.toml          | `bluz71/vim-nightfly-colors`, `extras/nightfly-wezterm.toml` |
//! | Oxocarbon            | Oxocarbon.toml         | iTerm2: `Oxocarbon.toml`              |
//! | Ferra                | Ferra.toml             | `casperstorm/ferra`, `ports/wezterm/ferra.toml` |

use std::sync::LazyLock;

use iced::theme::Base;

use crate::{Scheme, Theme};

/// The schemes in [`iced::Theme::ALL`] order. The order is the pairing: index
/// `n` of this table is the scheme of `iced::Theme::ALL[n]`.
const SCHEMES: [&str; 22] = [
    include_str!("../schemes/Light.toml"),
    include_str!("../schemes/Dark.toml"),
    include_str!("../schemes/Dracula.toml"),
    include_str!("../schemes/Nord.toml"),
    include_str!("../schemes/Solarized Light.toml"),
    include_str!("../schemes/Solarized Dark.toml"),
    include_str!("../schemes/Gruvbox Light.toml"),
    include_str!("../schemes/Gruvbox Dark.toml"),
    include_str!("../schemes/Catppuccin Latte.toml"),
    include_str!("../schemes/Catppuccin Frappe.toml"),
    include_str!("../schemes/Catppuccin Macchiato.toml"),
    include_str!("../schemes/Catppuccin Mocha.toml"),
    include_str!("../schemes/Tokyo Night.toml"),
    include_str!("../schemes/Tokyo Night Storm.toml"),
    include_str!("../schemes/Tokyo Night Light.toml"),
    include_str!("../schemes/Kanagawa Wave.toml"),
    include_str!("../schemes/Kanagawa Dragon.toml"),
    include_str!("../schemes/Kanagawa Lotus.toml"),
    include_str!("../schemes/Moonfly.toml"),
    include_str!("../schemes/Nightfly.toml"),
    include_str!("../schemes/Oxocarbon.toml"),
    include_str!("../schemes/Ferra.toml"),
];

static PACK: LazyLock<Vec<Theme>> = LazyLock::new(|| {
    iced::Theme::ALL
        .iter()
        .zip(SCHEMES)
        .map(|(base, source)| {
            // Compiled in and covered by a test: a failure here is a broken
            // build, not a broken input.
            let scheme = Scheme::from_wezterm(source).unwrap_or_else(|error| {
                panic!("bundled scheme of `{}`: {error}", Base::name(base))
            });

            Theme::paired(base.clone(), scheme)
        })
        .collect()
});

pub fn pack() -> &'static [Theme] {
    &PACK
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_iced_theme_is_paired_with_a_scheme() {
        let pack = pack();

        assert_eq!(pack.len(), 22);
        assert_eq!(pack.len(), iced::Theme::ALL.len());

        for (theme, base) in pack.iter().zip(iced::Theme::ALL) {
            assert_eq!(theme.name(), Base::name(base));
            assert_eq!(theme.base(), base);
        }
    }

    #[test]
    fn names_are_unique() {
        let names: HashSet<&str> = pack().iter().map(Theme::name).collect();

        assert_eq!(names.len(), pack().len());
    }

    #[test]
    fn a_scheme_has_the_tone_of_the_theme_it_is_paired_with() {
        // The pairing is positional (`SCHEMES[n]` belongs to
        // `iced::Theme::ALL[n]`), so a slipped entry is a silent wrong
        // pairing. Tone is what no mismatch survives: a light theme with a
        // dark terminal scheme is a black pane in a white editor.
        for theme in pack() {
            assert_eq!(
                iced::theme::palette::is_dark(theme.scheme().background),
                theme.is_dark(),
                "{}",
                theme.name()
            );
        }
    }
}
