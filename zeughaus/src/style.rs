//! Surface colours and style closures for the editor's chrome lists, panels and
//! buttons, ported from griasdi's desktop so the notice center and the CI page
//! look like it. [`Tokens`] derives the surfaces from the theme's palette; the
//! style functions are built from them.

use iced::theme::palette::{deviate, mix};
use iced::widget::{button, container};
use iced::{Background, Border, Color, Font, Shadow, Vector};
use zeughaus_theme::Theme;

/// Bold chrome text: titles, counts, headings.
pub const BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..crate::font::UI
};

/// Surface colours derived from a theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    /// The sidebar of a page and the notice center's panel.
    pub sidebar: Color,
    /// A page's main surface.
    pub surface: Color,
    /// A hovered or selected row.
    pub surface_hover: Color,
    /// Borders and separators.
    pub border: Color,
    /// Secondary text.
    pub muted_text: Color,
}

impl Tokens {
    /// The surfaces of `theme`, computed on its iced palette.
    pub fn of(theme: &Theme) -> Self {
        let p = theme.base().palette();
        let rail = deviate(p.background, 0.05);
        Self {
            sidebar: deviate(p.background, 0.03),
            surface: p.background,
            surface_hover: deviate(p.background, 0.07),
            border: deviate(p.background, 0.12),
            muted_text: muted(p.text, p.background, rail),
        }
    }
}

/// `text` mixed toward `background` as far as it keeps 4.5:1 (WCAG 1.4.3) on
/// `floor`, at most 60 %; `text` itself when no step does.
fn muted(text: Color, background: Color, floor: Color) -> Color {
    (1..=12)
        .rev()
        .map(|i| mix(text, background, i as f32 * 0.05))
        .find(|c| c.relative_contrast(floor) >= 4.5)
        .unwrap_or(text)
}

/// List entry button; `selected` entries keep the hover background.
pub fn channel_button(
    t: &Tokens,
    selected: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style + use<> {
    let t = *t;
    move |theme, status| {
        let text = theme.base().palette().text;
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = (selected || hovered).then_some(Background::Color(t.surface_hover));
        let text_color = match (selected, hovered, status) {
            (_, _, button::Status::Disabled) => t.muted_text,
            (true, _, _) | (_, true, _) => text,
            _ => t.muted_text,
        };
        button::Style {
            background,
            text_color,
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// Quiet bordered button for secondary actions.
pub fn secondary_button(t: &Tokens) -> impl Fn(&Theme, button::Status) -> button::Style + use<> {
    let t = *t;
    move |theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: hovered.then_some(Background::Color(t.surface_hover)),
            text_color: if status == button::Status::Disabled {
                t.muted_text
            } else {
                theme.base().palette().text
            },
            border: Border {
                color: t.border,
                width: 1.0,
                radius: 999.0.into(),
            },
            ..button::Style::default()
        }
    }
}

/// Card background with a border: one notice of the notice center.
pub fn card(t: &Tokens) -> impl Fn(&Theme) -> container::Style + use<> {
    let (bg, border) = (t.sidebar, t.border);
    move |_| container::Style {
        background: Some(Background::Color(bg)),
        border: Border {
            color: border,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

/// The notice center's panel along the window's right edge; `corner` rounds
/// the window's bottom-right corner.
pub fn notice_center(t: &Tokens, corner: f32) -> impl Fn(&Theme) -> container::Style + use<> {
    let t = *t;
    move |theme| container::Style {
        background: Some(Background::Color(t.sidebar)),
        text_color: Some(theme.base().palette().text),
        border: Border {
            color: t.border,
            width: 1.0,
            radius: iced::border::left(8.0).bottom_right(corner),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
            offset: Vector::new(-4.0, 0.0),
            blur_radius: 16.0,
        },
        ..container::Style::default()
    }
}

/// The notice center's pull tab, filled in the colour of its most severe
/// notice.
pub fn notice_tab(
    severity: crate::notices::Severity,
) -> impl Fn(&Theme, button::Status) -> button::Style + use<> {
    use crate::notices::Severity;
    move |theme, status| {
        let p = theme.extended();
        let (base, strong) = match severity {
            Severity::Error => (p.danger.base, p.danger.strong),
            Severity::Warning => (p.warning.base, p.warning.strong),
            Severity::Info => (p.primary.base, p.primary.strong),
        };
        let pair = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            strong
        } else {
            base
        };
        button::Style {
            background: Some(Background::Color(pair.color)),
            text_color: pair.text,
            border: Border {
                radius: iced::border::left(6.0),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// The colour of a notice's severity: the theme's danger, warning or primary.
pub fn severity_color(theme: &Theme, severity: crate::notices::Severity) -> Color {
    use crate::notices::Severity;
    let p = theme.extended();
    match severity {
        Severity::Error => p.danger.base.color,
        Severity::Warning => p.warning.base.color,
        Severity::Info => p.primary.base.color,
    }
}
