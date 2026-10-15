//! One theme for every surface the editor draws.
//!
//! A terminal colour scheme is the source: 16 ANSI slots plus foreground,
//! background, cursor and selection. Everything else is derived from it, so a
//! theme cannot disagree with itself -- the graph, the chrome and the terminal
//! panes all name colours out of the same sixteen.
//!
//! [`Theme`] carries both that [`Scheme`] and an [`iced::Theme`]. The iced
//! theme is what every widget catalog resolves against (this crate only
//! forwards), which keeps iced's own palettes exactly as they look upstream
//! for the bundled pack and derives one from the scheme for a theme a user
//! dropped in. A host bridges a widget hardwired to `iced::Theme` with
//! `iced::widget::themer(Some(theme.base().clone()), ..)`.

mod catalog;
mod nodegraph;
mod pack;
mod tabs;
#[cfg(feature = "terminal")]
mod terminal;
mod wezterm;

use std::sync::Arc;

use iced::Color;
use iced::theme::{Base, Mode, Palette};

pub use iced::theme::palette::Extended;
pub use wezterm::ParseError;

/// The 16 ANSI colours, in the order a terminal numbers them. The
/// discriminant is the index into [`Scheme::ansi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ansi {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

/// A terminal colour scheme: what a WezTerm `[colors]` table states.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scheme {
    /// The 16 ANSI colours, indexed by [`Ansi`].
    pub ansi: [Color; 16],
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection: Color,
}

impl Scheme {
    /// Reads a scheme from a WezTerm colour TOML.
    ///
    /// See [`Theme::from_wezterm`] for what the format has to carry.
    pub fn from_wezterm(toml: &str) -> Result<Scheme, ParseError> {
        wezterm::parse(toml)
    }

    /// One of the 16 ANSI colours.
    pub fn ansi(&self, color: Ansi) -> Color {
        self.ansi[color as usize]
    }

    /// The iced [`Palette`] this scheme stands for.
    ///
    /// The six colours iced builds a whole extended palette out of all come
    /// from the scheme: a themed widget and a themed terminal cell showing the
    /// same meaning show the same colour.
    pub fn palette(&self) -> Palette {
        Palette {
            background: self.background,
            text: self.foreground,
            primary: self.ansi(Ansi::Blue),
            success: self.ansi(Ansi::Green),
            warning: self.ansi(Ansi::Yellow),
            danger: self.ansi(Ansi::Red),
        }
    }
}

#[derive(Debug, PartialEq)]
struct Inner {
    name: String,
    base: iced::Theme,
    scheme: Scheme,
    /// Worked out once: [`Theme::muted`] is asked for on every frame.
    muted: Color,
}

impl Inner {
    fn new(name: String, base: iced::Theme, scheme: Scheme) -> Inner {
        let palette = base.extended_palette();
        let muted = muted(palette.background.base.text, palette.background.base.color);
        Inner {
            name,
            base,
            scheme,
            muted,
        }
    }
}

/// Contrast secondary text aims for against the canvas: the 4.5:1 floor for
/// body text, with room for the surfaces a step off the canvas.
const MUTED_CONTRAST: f32 = 5.0;
/// The floor itself, for a theme whose own text has no room to spare.
const BODY_CONTRAST: f32 = 4.5;

/// The foreground mixed towards the background as far as it can go and
/// still read: 40 % of the way, less where the theme's two colours are
/// close (a light theme mixes in linear light, which pales quickly). A
/// theme whose text is barely above the floor gets the faintest step that
/// still clears it, and one whose text is below it a token step, so that
/// secondary text stays told apart.
fn muted(text: Color, background: Color) -> Color {
    let mut floor = None;
    for step in (1..=8).rev() {
        let candidate = iced::theme::palette::mix(text, background, step as f32 * 0.05);
        let ratio = contrast(candidate, background);
        if ratio >= MUTED_CONTRAST {
            return candidate;
        }
        if ratio >= BODY_CONTRAST && floor.is_none() {
            floor = Some(candidate);
        }
    }
    floor.unwrap_or_else(|| iced::theme::palette::mix(text, background, 0.05))
}

/// The WCAG contrast ratio of two opaque colours.
fn contrast(a: Color, b: Color) -> f32 {
    let luminance = |c: Color| {
        let channel = |v: f32| {
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    };
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The theme of the whole editor.
///
/// Cloning is a refcount bump: `App::theme()` hands one out every frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme(Arc<Inner>);

impl Theme {
    /// The name shown in the palette and written to the settings file.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// The iced theme every widget catalog resolves against.
    pub fn base(&self) -> &iced::Theme {
        &self.0.base
    }

    /// The colour scheme this theme was built from.
    pub fn scheme(&self) -> &Scheme {
        &self.0.scheme
    }

    /// The extended palette of [`Theme::base`].
    pub fn extended(&self) -> &Extended {
        self.0.base.extended_palette()
    }

    pub fn is_dark(&self) -> bool {
        self.extended().is_dark
    }

    /// One of the 16 ANSI colours. The editor's own semantics (a pin's type, a
    /// node's category) are ANSI slots, so a scheme decides them too.
    pub fn ansi(&self, color: Ansi) -> Color {
        self.0.scheme.ansi(color)
    }

    /// The chrome behind title bars and the status bar: a shade off the
    /// canvas, never a colour of its own.
    pub fn chrome(&self) -> Color {
        self.extended().background.weak.color
    }

    /// Secondary text: labels, inactive pane titles. Foreground mixed towards
    /// the background, which stays readable in both tones where a fixed grey
    /// would not, and never below 4.5:1 against the canvas.
    pub fn muted(&self) -> Color {
        self.0.muted
    }

    /// What the eye is supposed to go to: focus, selection, the active pane.
    pub fn accent(&self) -> Color {
        self.extended().primary.base.color
    }

    /// A hint: something to know, not something wrong.
    pub fn hint(&self) -> Color {
        self.extended().warning.base.color
    }

    /// Something went wrong: a failed run, a refused setting, a broken edge.
    pub fn error(&self) -> Color {
        self.extended().danger.base.color
    }

    /// A theme whose widgets are derived from the scheme.
    ///
    /// This is what a scheme dropped into the state directory becomes: iced
    /// generates the extended palette out of [`Scheme::palette`].
    pub fn from_scheme(name: impl Into<String>, scheme: Scheme) -> Theme {
        let name = name.into();
        let base = iced::Theme::custom(name.clone(), scheme.palette());

        Theme(Arc::new(Inner::new(name, base, scheme)))
    }

    /// A theme that keeps iced's own palette for widgets and takes its ANSI
    /// colours from `scheme`.
    ///
    /// The bundled pack is built this way: iced's built-in themes are tuned
    /// for widgets and a derived palette would only make them worse, while the
    /// terminal needs sixteen colours no iced palette has. The name is the
    /// base theme's.
    pub fn paired(base: iced::Theme, scheme: Scheme) -> Theme {
        let name = Base::name(&base).to_string();

        Theme(Arc::new(Inner::new(name, base, scheme)))
    }

    /// Reads a theme from a WezTerm colour TOML.
    ///
    /// The `[colors]` table has to carry `ansi` and `brights` (eight `#rrggbb`
    /// entries each), `foreground` and `background`. `cursor_bg` falls back to
    /// the foreground and `selection_bg` to a foreground/background mix, since
    /// plenty of schemes in the wild state neither.
    pub fn from_wezterm(name: impl Into<String>, toml: &str) -> Result<Theme, ParseError> {
        Ok(Theme::from_scheme(name, Scheme::from_wezterm(toml)?))
    }

    /// The built-in themes: every [`iced::Theme::ALL`] entry paired with the
    /// terminal colour scheme of the same name, in that order.
    pub fn pack() -> &'static [Theme] {
        pack::pack()
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::pack()[1].clone()
    }
}

impl Base for Theme {
    fn default(preference: Mode) -> Self {
        match preference {
            Mode::Dark => Theme::pack()[1].clone(),
            Mode::None | Mode::Light => Theme::pack()[0].clone(),
        }
    }

    fn mode(&self) -> Mode {
        Base::mode(&self.0.base)
    }

    fn base(&self) -> iced::theme::Style {
        Base::base(&self.0.base)
    }

    fn palette(&self) -> Option<Palette> {
        Some(self.0.base.palette())
    }

    fn name(&self) -> &str {
        &self.0.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_indexes_the_scheme() {
        let scheme = Theme::pack()[2].scheme();

        assert_eq!(scheme.ansi(Ansi::Black), scheme.ansi[0]);
        assert_eq!(scheme.ansi(Ansi::White), scheme.ansi[7]);
        assert_eq!(scheme.ansi(Ansi::BrightBlack), scheme.ansi[8]);
        assert_eq!(scheme.ansi(Ansi::BrightWhite), scheme.ansi[15]);
    }

    #[test]
    fn palette_is_derived_from_the_scheme() {
        let scheme = Scheme::from_wezterm(include_str!("../schemes/Dracula.toml")).unwrap();
        let palette = scheme.palette();

        assert_eq!(palette.background, scheme.background);
        assert_eq!(palette.text, scheme.foreground);
        assert_eq!(palette.primary, scheme.ansi[4]);
        assert_eq!(palette.success, scheme.ansi[2]);
        assert_eq!(palette.warning, scheme.ansi[3]);
        assert_eq!(palette.danger, scheme.ansi[1]);
    }

    #[test]
    fn a_derived_theme_names_itself_and_keeps_its_scheme() {
        let scheme = Scheme::from_wezterm(include_str!("../schemes/Nord.toml")).unwrap();
        let theme = Theme::from_scheme("Dropped In", scheme);

        assert_eq!(theme.name(), "Dropped In");
        assert_eq!(Base::name(&theme), "Dropped In");
        assert_eq!(theme.base().palette(), scheme.palette());
        assert_eq!(*theme.scheme(), scheme);
    }

    #[test]
    fn a_paired_theme_keeps_iceds_palette() {
        let theme = &Theme::pack()[3];

        assert_eq!(theme.name(), "Nord");
        assert_eq!(theme.base().palette(), iced::Theme::Nord.palette());
        assert_eq!(*theme.extended(), *iced::Theme::Nord.extended_palette());
        // What `paired` is for: the widgets keep iced's tuned palette instead
        // of one derived from the terminal scheme.
        assert_ne!(theme.base().palette(), theme.scheme().palette());
    }

    #[test]
    fn default_is_dark() {
        let dark = <Theme as Default>::default();

        assert_eq!(dark.name(), "Dark");
        assert!(dark.is_dark());
        assert_eq!(<Theme as Base>::default(Mode::Light).name(), "Light");
        assert_eq!(<Theme as Base>::default(Mode::None).name(), "Light");
    }

    /// Secondary text stays readable in every bundled theme, light ones
    /// included, and stays told apart from the text, also in a theme whose
    /// own text is barely above the floor (Solarized Light).
    #[test]
    fn muted_text_keeps_body_contrast_in_every_bundled_theme() {
        for theme in Theme::pack() {
            let background = theme.extended().background.base.color;
            let text = contrast(theme.extended().background.base.text, background);
            let ratio = contrast(theme.muted(), background);
            if text >= BODY_CONTRAST + 0.2 {
                assert!(
                    ratio >= BODY_CONTRAST,
                    "{}: muted at {ratio:.2}:1",
                    theme.name()
                );
            }
            assert!(
                ratio < text,
                "{}: muted is as strong as the text",
                theme.name()
            );
        }
    }
}
