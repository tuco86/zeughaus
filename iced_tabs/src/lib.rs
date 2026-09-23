use std::borrow::Cow;

use iced::widget::{Column, Row, button, column, container, row, scrollable, text};
use iced::{Background, Border, Color, Element, Length};

/// Where a tab bar is placed relative to its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    Top,
    Left,
}

/// One tab shown by [`view`].
#[derive(Debug, Clone)]
pub struct Tab<'a, Id> {
    pub id: Id,
    pub label: Cow<'a, str>,
    pub group: Option<Cow<'a, str>>,
    pub accent: Option<Color>,
    pub closable: bool,
}

impl<'a, Id> Tab<'a, Id> {
    pub fn new(id: Id, label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            id,
            label: label.into(),
            group: None,
            accent: None,
            closable: true,
        }
    }

    pub fn group(mut self, group: impl Into<Cow<'a, str>>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn accent(mut self, accent: Color) -> Self {
        self.accent = Some(accent);
        self
    }

    pub fn closable(mut self, closable: bool) -> Self {
        self.closable = closable;
        self
    }
}

/// The state of a tab when it is styled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    /// Whether the tab is the active one.
    pub selected: bool,
    /// Whether the cursor is over the tab.
    pub hovered: bool,
    /// Whether the tab is being pressed.
    pub pressed: bool,
    /// The accent of the tab, if it has one.
    pub accent: Option<Color>,
}

/// The appearance of a tab.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// The background behind the tab.
    pub background: Option<Background>,
    /// The colour of the tab label.
    pub text: Color,
    /// The border around the tab.
    pub border: Border,
}

/// A styling function for a tab.
pub type StyleFn<'a, Theme> = Box<dyn Fn(&Theme, Status) -> Style + 'a>;

/// The theme catalog of a tab bar.
pub trait Catalog {
    /// The item class of the [`Catalog`].
    type Class<'a>;

    /// The default class produced by the [`Catalog`].
    fn default<'a>() -> Self::Class<'a>;

    /// The [`Style`] of a class with the given status.
    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style;
}

impl Catalog for iced::Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(default)
    }

    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style {
        class(self, status)
    }
}

/// The default tab style: a primary button when selected, a text button
/// otherwise, with the accent as the border.
pub fn default(theme: &iced::Theme, status: Status) -> Style {
    let button_status = if status.pressed {
        button::Status::Pressed
    } else if status.hovered {
        button::Status::Hovered
    } else {
        button::Status::Active
    };

    let base = if status.selected {
        button::primary(theme, button_status)
    } else {
        button::text(theme, button_status)
    };

    Style {
        background: base.background,
        text: base.text_color,
        border: match status.accent {
            Some(accent) => Border {
                color: accent,
                width: if status.selected { 2.0 } else { 1.0 },
                radius: 4.0.into(),
            },
            None => base.border,
        },
    }
}

struct Group<'a, Id> {
    label: Option<Cow<'a, str>>,
    tabs: Vec<Tab<'a, Id>>,
}

/// Builds a grouped tab bar from application-owned tab state.
///
/// The widget owns no selection or ordering state. Activating and closing tabs
/// produces messages and the caller supplies the resulting state on the next
/// view pass.
pub fn view<'a, Id, Message, Theme>(
    tabs: impl IntoIterator<Item = Tab<'a, Id>>,
    active: Id,
    placement: Placement,
    on_activate: impl Fn(Id) -> Message,
    on_close: impl Fn(Id) -> Message,
) -> Element<'a, Message, Theme>
where
    Id: Copy + Eq + 'a,
    Message: Clone + 'a,
    Theme:
        Catalog + button::Catalog + text::Catalog + container::Catalog + scrollable::Catalog + 'a,
    <Theme as Catalog>::Class<'a>: 'a,
    <Theme as button::Catalog>::Class<'a>: From<button::StyleFn<'a, Theme>>,
{
    let mut groups: Vec<Group<'a, Id>> = Vec::new();

    for tab in tabs {
        let continues_group = groups
            .last()
            .is_some_and(|group| group.label.as_deref() == tab.group.as_deref());

        if continues_group {
            groups.last_mut().expect("group exists").tabs.push(tab);
        } else {
            groups.push(Group {
                label: tab.group.clone(),
                tabs: vec![tab],
            });
        }
    }

    match placement {
        Placement::Top => {
            let mut sections = Row::new().spacing(10);
            for Group { label: group, tabs } in groups {
                let mut items = Row::new().spacing(2);
                for tab in tabs {
                    items = items.push(tab_item(tab, active, false, &on_activate, &on_close));
                }

                let mut section = Column::new().spacing(2);
                if let Some(group) = group {
                    section = section.push(text(group.into_owned()).size(10));
                }
                sections = sections.push(section.push(items));
            }

            container(
                scrollable(sections)
                    .direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::new(),
                    ))
                    .width(Length::Fill),
            )
            .width(Length::Fill)
            .padding([3, 4])
            .into()
        }
        Placement::Left => {
            let mut sections = Column::new().spacing(10);
            for Group { label: group, tabs } in groups {
                let mut items = Column::new().spacing(2);
                for tab in tabs {
                    items = items.push(tab_item(tab, active, true, &on_activate, &on_close));
                }

                let mut section = column![].spacing(2);
                if let Some(group) = group {
                    section = section.push(text(group.into_owned()).size(10));
                }
                sections = sections.push(section.push(items));
            }

            container(
                scrollable(sections)
                    .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new()))
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([4, 3])
            .into()
        }
    }
}

fn tab_item<'a, Id, Message, Theme>(
    tab: Tab<'a, Id>,
    active: Id,
    fill: bool,
    on_activate: &impl Fn(Id) -> Message,
    on_close: &impl Fn(Id) -> Message,
) -> Element<'a, Message, Theme>
where
    Id: Copy + Eq,
    Message: Clone + 'a,
    Theme:
        Catalog + button::Catalog + text::Catalog + container::Catalog + scrollable::Catalog + 'a,
    <Theme as Catalog>::Class<'a>: 'a,
    <Theme as button::Catalog>::Class<'a>: From<button::StyleFn<'a, Theme>>,
{
    let selected = tab.id == active;
    let accent = tab.accent;
    // One class per button: resolving it per draw would allocate every frame.
    let label_class: <Theme as Catalog>::Class<'a> = <Theme as Catalog>::default();
    let close_class: <Theme as Catalog>::Class<'a> = <Theme as Catalog>::default();

    let mut activate = button(text(tab.label.into_owned()).size(13))
        .padding([5, 8])
        .on_press(on_activate(tab.id))
        .style(move |theme: &Theme, status| {
            button_style(theme, &label_class, status, selected, accent)
        });

    if fill {
        activate = activate.width(Length::Fill);
    }

    let mut item = row![activate].spacing(1);
    if tab.closable {
        item = item.push(
            button(text("x").size(11))
                .padding([5, 6])
                .on_press(on_close(tab.id))
                .style(move |theme: &Theme, status| {
                    button_style(theme, &close_class, status, false, None)
                }),
        );
    }

    container(item)
        .width(if fill { Length::Fill } else { Length::Shrink })
        .into()
}

fn button_style<Theme>(
    theme: &Theme,
    class: &<Theme as Catalog>::Class<'_>,
    status: button::Status,
    selected: bool,
    accent: Option<Color>,
) -> button::Style
where
    Theme: Catalog,
{
    let style = Catalog::style(
        theme,
        class,
        Status {
            selected,
            hovered: matches!(status, button::Status::Hovered),
            pressed: matches!(status, button::Status::Pressed),
            accent,
        },
    );

    button::Style {
        background: style.background,
        text_color: style.text,
        border: style.border,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(theme: &iced::Theme, status: Status) -> Style {
        let class = <iced::Theme as Catalog>::default();
        Catalog::style(theme, &class, status)
    }

    fn status(selected: bool, accent: Option<Color>) -> Status {
        Status {
            selected,
            hovered: false,
            pressed: false,
            accent,
        }
    }

    #[test]
    fn selected_tab_is_primary_and_unselected_is_transparent() {
        let theme = iced::Theme::Dark;
        let palette = theme.extended_palette();

        let selected = style(&theme, status(true, None));
        assert_eq!(
            selected.background,
            Some(Background::Color(palette.primary.base.color))
        );
        assert_eq!(selected.text, palette.primary.base.text);

        let unselected = style(&theme, status(false, None));
        assert_eq!(unselected.background, None);
        assert_eq!(unselected.text, palette.background.base.text);
    }

    #[test]
    fn accent_becomes_the_border_and_thickens_when_selected() {
        let theme = iced::Theme::Dark;
        let accent = Color::from_rgb(1.0, 0.0, 0.5);

        let selected = style(&theme, status(true, Some(accent)));
        assert_eq!(selected.border.color, accent);
        assert_eq!(selected.border.width, 2.0);

        let unselected = style(&theme, status(false, Some(accent)));
        assert_eq!(unselected.border.color, accent);
        assert_eq!(unselected.border.width, 1.0);
    }

    #[test]
    fn pressed_and_hovered_reach_the_style() {
        let theme = iced::Theme::Dark;
        let palette = theme.extended_palette();

        let hovered = style(
            &theme,
            Status {
                hovered: true,
                ..status(true, None)
            },
        );
        assert_eq!(
            hovered.background,
            Some(Background::Color(palette.primary.strong.color))
        );

        let pressed = style(
            &theme,
            Status {
                pressed: true,
                ..status(true, None)
            },
        );
        assert_eq!(
            pressed.background,
            Some(Background::Color(palette.primary.base.color))
        );
    }

    #[test]
    fn view_builds_with_the_iced_theme() {
        #[derive(Debug, Clone, PartialEq)]
        enum Message {
            Activate(u8),
            Close(u8),
        }

        let _: Element<'_, Message, iced::Theme> = view(
            [
                Tab::new(0u8, "one").group("group"),
                Tab::new(1u8, "two").accent(Color::WHITE).closable(false),
            ],
            0u8,
            Placement::Left,
            Message::Activate,
            Message::Close,
        );
    }
}
