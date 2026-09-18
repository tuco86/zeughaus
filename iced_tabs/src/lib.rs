use std::borrow::Cow;

use iced::widget::{Column, Row, button, column, container, row, scrollable, text};
use iced::{Border, Color, Element, Length, Theme};

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

struct Group<'a, Id> {
    label: Option<Cow<'a, str>>,
    tabs: Vec<Tab<'a, Id>>,
}

/// Builds a grouped tab bar from application-owned tab state.
///
/// The widget owns no selection or ordering state. Activating and closing tabs
/// produces messages and the caller supplies the resulting state on the next
/// view pass.
pub fn view<'a, Id, Message>(
    tabs: impl IntoIterator<Item = Tab<'a, Id>>,
    active: Id,
    placement: Placement,
    on_activate: impl Fn(Id) -> Message,
    on_close: impl Fn(Id) -> Message,
) -> Element<'a, Message>
where
    Id: Copy + Eq + 'a,
    Message: Clone + 'a,
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

fn tab_item<'a, Id, Message>(
    tab: Tab<'a, Id>,
    active: Id,
    fill: bool,
    on_activate: &impl Fn(Id) -> Message,
    on_close: &impl Fn(Id) -> Message,
) -> Element<'a, Message>
where
    Id: Copy + Eq,
    Message: Clone + 'a,
{
    let selected = tab.id == active;
    let accent = tab.accent;
    let mut activate = button(text(tab.label.into_owned()).size(13))
        .padding([5, 8])
        .on_press(on_activate(tab.id))
        .style(move |theme: &Theme, status| {
            let mut style = if selected {
                button::primary(theme, status)
            } else {
                button::text(theme, status)
            };
            if let Some(accent) = accent {
                style.border = Border {
                    color: accent,
                    width: if selected { 2.0 } else { 1.0 },
                    radius: 4.0.into(),
                };
            }
            style
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
                .style(button::text),
        );
    }

    container(item)
        .width(if fill { Length::Fill } else { Length::Shrink })
        .into()
}
