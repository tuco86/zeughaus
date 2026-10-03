//! A tab tree: sections of loose tabs and one level of coloured, collapsible
//! groups, drawn as a sidebar or as one row along the top of a window.
//!
//! The widget owns no selection, order, collapse, rename or drag state.
//! Everything it shows comes from the caller on every view pass, and every
//! interaction is a message built by the caller's [`Handlers`]. A tab body
//! reports the press itself, not a click: the caller decides on release
//! whether the press was a click (activate) or the start of a drag, and
//! feeds the hovered [`Target`] back as the [`Marker`] of the next pass.

use std::borrow::Cow;
use std::cell::RefCell;
use std::marker::PhantomData;

use iced_widget::core::layout::{self, Layout};
use iced_widget::core::widget::{self, Tree, Widget, tree};
use iced_widget::core::{
    Alignment, Background, Border, Color, Event, Length, Padding, Point, Rectangle, Size, Vector,
    border, touch, window,
};
use iced_widget::core::{Clipboard, Shell, mouse, overlay, renderer};
use iced_widget::text::Wrapping;
use iced_widget::{
    Button, Column, Row, Theme, button, container, mouse_area, scrollable, space, text,
};

/// An element whose renderer defaults to iced's, as `iced::Element` does.
type Element<'a, Message, Theme, Renderer = iced_widget::Renderer> =
    iced_widget::core::Element<'a, Message, Theme, Renderer>;

/// Where a tab bar is placed relative to its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    Top,
    Left,
}

/// Characters of a label shown before it is cut off with an ellipsis: a
/// terminal that names itself after a deep path must not crowd out the
/// other tabs.
pub const MAX_LABEL_CHARS: usize = 32;

/// Thickness of the gap before every item, drawn in the marker colour when
/// a drop would land there. The gap is always there, so showing the marker
/// never moves an item out from under the cursor.
const SLOT: f32 = 2.0;

/// Depth of the drop zone after the last item of a section or group.
const END_ZONE: f32 = 12.0;

/// Depth of the drop zone before a root group: a group has no body of its
/// own to hover, so the gap before it has to be deep enough to aim at.
const GROUP_ENTRY: f32 = 8.0;

/// One tab shown by [`tree`].
#[derive(Debug, Clone)]
pub struct Tab<'a, Id> {
    pub id: Id,
    pub label: Cow<'a, str>,
    pub accent: Option<Color>,
    pub closable: bool,
}

impl<'a, Id> Tab<'a, Id> {
    pub fn new(id: Id, label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            id,
            label: label.into(),
            accent: None,
            closable: true,
        }
    }

    /// A colour the tab carries as a dot before its label.
    pub fn accent(mut self, accent: Color) -> Self {
        self.accent = Some(accent);
        self
    }

    pub fn closable(mut self, closable: bool) -> Self {
        self.closable = closable;
        self
    }
}

/// A named, coloured run of tabs inside a section. Groups do not nest.
pub struct Group<'a, Id, G, Message, Theme> {
    pub id: G,
    pub label: Cow<'a, str>,
    pub color: Color,
    /// A locked group cannot be recoloured or dissolved from its header.
    pub locked: bool,
    /// A collapsed group shows only its header.
    pub collapsed: bool,
    pub tabs: Vec<Tab<'a, Id>>,
    /// Replaces the label while the app renames the group (a `text_input`
    /// it owns).
    pub editor: Option<Element<'a, Message, Theme>>,
}

/// One entry of a section: a loose tab or a group of tabs.
pub enum Item<'a, Id, G, Message, Theme> {
    Tab(Tab<'a, Id>),
    Group(Group<'a, Id, G, Message, Theme>),
}

/// The outer level of the tree: a header with controls and its items.
pub struct Section<'a, Id, G, S, Message, Theme> {
    pub id: S,
    pub label: Cow<'a, str>,
    /// A collapsed section shows only its header.
    pub collapsed: bool,
    /// Buttons in the header: a key handed back through
    /// [`Handlers::on_control`] and the label shown.
    pub controls: Vec<(&'static str, Cow<'a, str>)>,
    pub items: Vec<Item<'a, Id, G, Message, Theme>>,
}

/// Where a dragged item would land.
#[derive(Debug, Clone, PartialEq)]
pub enum Target<G, S> {
    /// Before the item at `index` of a container: the section's root items
    /// (groups count as one item each) when `group` is `None`, else the
    /// group's tabs.
    Before {
        section: S,
        group: Option<G>,
        index: usize,
    },
    /// After the last tab of a group, collapsed or not.
    GroupEnd(S, G),
    /// After the last root item of a section.
    SectionEnd(S),
}

/// The drop position the tree shows: a line before an item, a highlighted
/// group header or a highlighted section end zone.
pub type Marker<G, S> = Target<G, S>;

/// The messages the tree produces.
pub struct Handlers<'a, Id, G, S, Message> {
    /// The close button of a tab.
    pub on_close: Box<dyn Fn(Id) -> Message + 'a>,
    /// The chevron of a section header.
    pub on_toggle_section: Box<dyn Fn(S) -> Message + 'a>,
    /// The chevron of a group header.
    pub on_toggle_group: Box<dyn Fn(S, G) -> Message + 'a>,
    /// A control button of a section header, with its key.
    pub on_control: Box<dyn Fn(S, &'static str) -> Message + 'a>,
    /// The left button went down on a tab body (not on its close button).
    pub on_press_tab: Box<dyn Fn(Id) -> Message + 'a>,
    /// The left button went down on a group header (not on its buttons).
    pub on_press_group: Box<dyn Fn(S, G) -> Message + 'a>,
    /// The cursor entered a tab, a group header or an end zone.
    pub on_hover: Box<dyn Fn(Target<G, S>) -> Message + 'a>,
    /// A double click on a group's name.
    pub on_rename_group: Box<dyn Fn(S, G) -> Message + 'a>,
    /// A double click on a tab.
    pub on_rename_tab: Box<dyn Fn(Id) -> Message + 'a>,
    /// The colour dot of an unlocked group.
    pub on_cycle_color: Box<dyn Fn(S, G) -> Message + 'a>,
    /// The close button of an unlocked group.
    pub on_dissolve_group: Box<dyn Fn(S, G) -> Message + 'a>,
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
    /// Where the bar sits, which decides the tab's shape.
    pub placement: Placement,
}

/// The appearance of a tab, a header or a group's background.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// The background behind the element.
    pub background: Option<Background>,
    /// The colour of its text and buttons.
    pub text: Color,
    /// The border around it.
    pub border: Border,
}

/// A styling function for a tab.
pub type StyleFn<'a, Theme> = Box<dyn Fn(&Theme, Status) -> Style + 'a>;

/// The theme catalog of a tab tree.
pub trait Catalog {
    /// The item class of the [`Catalog`].
    type Class<'a>;

    /// The default class produced by the [`Catalog`].
    fn default<'a>() -> Self::Class<'a>;

    /// The [`Style`] of a tab of the given class and status.
    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style;

    /// The header of a section.
    fn section_header(&self, placement: Placement) -> Style;

    /// The header of a group of the given colour; `marked` while a drop
    /// would append to the group.
    fn group_header(&self, color: Color, marked: bool, placement: Placement) -> Style;

    /// The area behind a group's header and tabs.
    fn group_background(&self, color: Color, placement: Placement) -> Style;

    /// The colour of the drop marker.
    fn marker(&self) -> Color;
}

impl Catalog for Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(default)
    }

    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style {
        class(self, status)
    }

    fn section_header(&self, placement: Placement) -> Style {
        section_header(self, placement)
    }

    fn group_header(&self, color: Color, marked: bool, placement: Placement) -> Style {
        group_header(self, color, marked, placement)
    }

    fn group_background(&self, color: Color, placement: Placement) -> Style {
        group_background(self, color, placement)
    }

    fn marker(&self) -> Color {
        marker(self)
    }
}

/// Selected tabs join the content background, on top and at the left alike:
/// the bar sits on the chrome, and the tab is the part of the content that
/// reaches into it. Only the corners away from the content are rounded.
/// Inactive tabs are dimmed, with a subtle background on hover.
pub fn default(theme: &Theme, status: Status) -> Style {
    let content = theme.extended_palette().background.base;
    let background = if status.selected {
        Some(content.color)
    } else if status.hovered || status.pressed {
        Some(Color {
            a: 0.5,
            ..content.color
        })
    } else {
        None
    };
    let text = if status.selected || status.hovered {
        content.text
    } else {
        Color {
            a: 0.65,
            ..content.text
        }
    };
    Style {
        background: background.map(Background::Color),
        text,
        border: Border {
            radius: away_from_content(status.placement),
            ..Border::default()
        },
    }
}

/// A section header is plain text on the chrome, a little dimmer than an
/// active tab.
pub fn section_header(theme: &Theme, _placement: Placement) -> Style {
    let content = theme.extended_palette().background.base;
    Style {
        background: None,
        text: Color {
            a: 0.8,
            ..content.text
        },
        border: Border::default(),
    }
}

/// A group header sits on its group's background; while a drop would
/// append to the group it is filled with the marker colour.
pub fn group_header(theme: &Theme, _color: Color, marked: bool, _placement: Placement) -> Style {
    Style {
        background: marked.then(|| {
            Background::Color(Color {
                a: 0.35,
                ..marker(theme)
            })
        }),
        text: theme.extended_palette().background.base.text,
        border: border::rounded(4),
    }
}

/// The group colour at a quarter of full opacity, whatever alpha the colour
/// carries, rounded like the tabs it holds.
pub fn group_background(theme: &Theme, color: Color, placement: Placement) -> Style {
    Style {
        background: Some(Background::Color(Color { a: 0.25, ..color })),
        text: theme.extended_palette().background.base.text,
        border: Border {
            radius: away_from_content(placement),
            ..Border::default()
        },
    }
}

/// The drop marker is the theme's primary colour.
pub fn marker(theme: &Theme) -> Color {
    theme.extended_palette().primary.base.color
}

/// Rounds only the corners away from the content the bar borders.
fn away_from_content(placement: Placement) -> border::Radius {
    match placement {
        Placement::Top => border::top(6),
        Placement::Left => border::left(6),
    }
}

/// Builds the tab tree from application-owned state.
///
/// On top the tree is one row as wide as its content (scrolling sideways
/// once it does not fit) and as tall as it is given, with the tabs standing
/// on its bottom edge; at the left it fills its column and scrolls
/// vertically. The bar's empty rest after the last section hovers that
/// section's end.
///
/// `renaming` is the tab being renamed and the field shown instead of its
/// label.
pub fn tree<'a, Id, G, S, Message, Theme>(
    sections: Vec<Section<'a, Id, G, S, Message, Theme>>,
    active: Option<Id>,
    placement: Placement,
    marker: Option<Marker<G, S>>,
    renaming: Option<(Id, Element<'a, Message, Theme>)>,
    handlers: Handlers<'a, Id, G, S, Message>,
) -> Element<'a, Message, Theme>
where
    Id: Clone + PartialEq + 'a,
    G: Clone + PartialEq + 'a,
    S: Clone + PartialEq + 'a,
    Message: Clone + 'a,
    Theme:
        Catalog + button::Catalog + text::Catalog + container::Catalog + scrollable::Catalog + 'a,
    <Theme as Catalog>::Class<'a>: 'a,
    <Theme as button::Catalog>::Class<'a>: From<button::StyleFn<'a, Theme>>,
    <Theme as container::Catalog>::Class<'a>: From<container::StyleFn<'a, Theme>>,
{
    let builder = Builder {
        handlers: &handlers,
        active,
        placement,
        marker,
        renaming: RefCell::new(renaming),
        theme: PhantomData,
    };
    let fill = sections.last().map(|last| {
        mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_enter((handlers.on_hover)(Target::SectionEnd(last.id.clone())))
    });
    let sections: Vec<Element<'a, Message, Theme>> = sections
        .into_iter()
        .map(|section| builder.section(section))
        .collect();

    match placement {
        Placement::Top => {
            // The scrollable shrinks to its row and scrolls once the row
            // outgrows the bar; the fill zone takes what is left.
            let mut row = Row::new()
                .push(
                    scrollable(
                        Row::with_children(sections)
                            .spacing(12)
                            .height(Length::Fill),
                    )
                    .direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::hidden(),
                    ))
                    .height(Length::Fill),
                )
                .height(Length::Fill);
            if let Some(fill) = fill {
                row = row.push(fill);
            }
            container(row)
                .height(Length::Fill)
                .padding(Padding {
                    top: 5.0,
                    ..Padding::ZERO
                })
                .into()
        }
        Placement::Left => {
            let mut column = Column::new()
                .push(
                    scrollable(
                        Column::with_children(sections)
                            .spacing(6)
                            .width(Length::Fill),
                    )
                    .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new()))
                    .height(Length::Shrink),
                )
                .width(Length::Fill)
                .height(Length::Fill);
            if let Some(fill) = fill {
                column = column.push(fill);
            }
            container(column)
                .width(Length::Fill)
                .height(Length::Fill)
                // No right padding: a selected tab touches the content it joins.
                .padding(Padding {
                    top: 4.0,
                    bottom: 4.0,
                    left: 3.0,
                    right: 0.0,
                })
                .into()
        }
    }
}

/// Whether `at` lies in the half of `bounds` that comes later in the bar's
/// direction.
fn trailing_half(placement: Placement, bounds: Rectangle, at: Point) -> bool {
    match placement {
        Placement::Top => at.x > bounds.center_x(),
        Placement::Left => at.y > bounds.center_y(),
    }
}

/// Whether `marker` puts the drop line before item `index` of the container
/// named by `section` and `group` (`None`: the section's root items).
fn marks_slot<G: PartialEq, S: PartialEq>(
    marker: Option<&Marker<G, S>>,
    section: &S,
    group: Option<&G>,
    index: usize,
) -> bool {
    matches!(
        marker,
        Some(Target::Before { section: s, group: g, index: i })
            if s == section && g.as_ref() == group && *i == index
    )
}

/// Whether `marker` highlights the header of `group` in `section`.
fn marks_group<G: PartialEq, S: PartialEq>(
    marker: Option<&Marker<G, S>>,
    section: &S,
    group: &G,
) -> bool {
    matches!(marker, Some(Target::GroupEnd(s, g)) if s == section && g == group)
}

/// Whether `marker` highlights the end zone of `section`.
fn marks_section_end<G, S: PartialEq>(marker: Option<&Marker<G, S>>, section: &S) -> bool {
    matches!(marker, Some(Target::SectionEnd(s)) if s == section)
}

fn chevron(collapsed: bool) -> &'static str {
    if collapsed { "\u{25B8}" } else { "\u{25BE}" }
}

/// The view pass state shared by every item of one tree.
struct Builder<'h, 'a, Id, G, S, Message, Theme> {
    handlers: &'h Handlers<'a, Id, G, S, Message>,
    active: Option<Id>,
    placement: Placement,
    marker: Option<Marker<G, S>>,
    /// Taken by the one tab it names.
    renaming: RefCell<Option<(Id, Element<'a, Message, Theme>)>>,
    theme: PhantomData<Theme>,
}

impl<'a, Id, G, S, Message, Theme> Builder<'_, 'a, Id, G, S, Message, Theme>
where
    Id: Clone + PartialEq + 'a,
    G: Clone + PartialEq + 'a,
    S: Clone + PartialEq + 'a,
    Message: Clone + 'a,
    Theme:
        Catalog + button::Catalog + text::Catalog + container::Catalog + scrollable::Catalog + 'a,
    <Theme as Catalog>::Class<'a>: 'a,
    <Theme as button::Catalog>::Class<'a>: From<button::StyleFn<'a, Theme>>,
    <Theme as container::Catalog>::Class<'a>: From<container::StyleFn<'a, Theme>>,
{
    fn section(
        &self,
        section: Section<'a, Id, G, S, Message, Theme>,
    ) -> Element<'a, Message, Theme> {
        let Section {
            id,
            label,
            collapsed,
            controls,
            items,
        } = section;
        let header = self.section_header(&id, label, collapsed, controls);
        if collapsed {
            return header;
        }

        let mut children = Vec::with_capacity(2 * items.len() + 2);
        children.push(header);
        for (index, item) in items.into_iter().enumerate() {
            let before = Target::Before {
                section: id.clone(),
                group: None,
                index,
            };
            match item {
                Item::Tab(tab) => {
                    let after = Target::Before {
                        section: id.clone(),
                        group: None,
                        index: index + 1,
                    };
                    children.push(self.slot(marks_slot(self.marker.as_ref(), &id, None, index)));
                    children.push(self.tab(tab, before, after));
                }
                Item::Group(group) => {
                    let marked = marks_slot(self.marker.as_ref(), &id, None, index);
                    children.push(self.entry_zone(before, marked));
                    children.push(self.group(&id, group));
                }
            }
        }
        let marked = marks_section_end(self.marker.as_ref(), &id);
        children.push(self.end_zone(Target::SectionEnd(id), marked));

        match self.placement {
            Placement::Top => Row::with_children(children).height(Length::Fill).into(),
            Placement::Left => Column::with_children(children).width(Length::Fill).into(),
        }
    }

    /// Chevron, label and one button per control. In the sidebar the
    /// controls get a row of their own, wrapping when they outgrow it: next
    /// to the label they would leave it no width at all.
    fn section_header(
        &self,
        id: &S,
        label: Cow<'a, str>,
        collapsed: bool,
        controls: Vec<(&'static str, Cow<'a, str>)>,
    ) -> Element<'a, Message, Theme> {
        let placement = self.placement;
        let text_color = move |theme: &Theme| Catalog::section_header(theme, placement).text;

        let mut title = Row::new().spacing(4).align_y(Alignment::Center);
        title = title.push(
            flat_button(
                text(chevron(collapsed)).size(12).line_height(1.0),
                text_color,
            )
            .padding([1, 3])
            .on_press((self.handlers.on_toggle_section)(id.clone())),
        );
        title = title.push(
            container(text(shorten(label)).size(12).wrapping(Wrapping::None))
                .width(self.fill_left())
                .clip(true),
        );
        let mut buttons = Row::new().spacing(4).align_y(Alignment::Center);
        for (key, label) in controls {
            buttons = buttons.push(
                flat_button(text(label).size(11).wrapping(Wrapping::None), text_color)
                    .padding([1, 5])
                    .on_press((self.handlers.on_control)(id.clone(), key)),
            );
        }
        let content: Element<'a, Message, Theme> = match placement {
            Placement::Top => title.push(buttons).into(),
            Placement::Left => Column::new()
                .push(title)
                .push(
                    container(buttons.wrap().vertical_spacing(2.0)).padding(Padding {
                        left: 18.0,
                        ..Padding::ZERO
                    }),
                )
                .spacing(2)
                .into(),
        };

        self.header(content)
            .style(move |theme: &Theme| container_style(Catalog::section_header(theme, placement)))
            .into()
    }

    /// Header and, unless collapsed, the tabs and the end zone, all on the
    /// group's background.
    fn group(
        &self,
        section: &S,
        group: Group<'a, Id, G, Message, Theme>,
    ) -> Element<'a, Message, Theme> {
        let Group {
            id,
            label,
            color,
            locked,
            collapsed,
            tabs,
            editor,
        } = group;
        let placement = self.placement;

        let mut children = Vec::with_capacity(2 * tabs.len() + 2);
        children.push(self.group_header(section, &id, label, color, locked, collapsed, editor));
        if !collapsed {
            for (index, tab) in tabs.into_iter().enumerate() {
                children.push(self.slot(marks_slot(
                    self.marker.as_ref(),
                    section,
                    Some(&id),
                    index,
                )));
                let before = |index| Target::Before {
                    section: section.clone(),
                    group: Some(id.clone()),
                    index,
                };
                children.push(self.tab(tab, before(index), before(index + 1)));
            }
            children.push(self.end_zone(Target::GroupEnd(section.clone(), id), false));
        }

        let (body, padding): (Element<'a, Message, Theme>, _) = match placement {
            Placement::Top => (
                Row::with_children(children).height(Length::Fill).into(),
                Padding {
                    left: 4.0,
                    ..Padding::ZERO
                },
            ),
            // No right padding: a selected tab touches the content it joins.
            Placement::Left => (
                Column::with_children(children).width(Length::Fill).into(),
                Padding {
                    top: 2.0,
                    left: 6.0,
                    ..Padding::ZERO
                },
            ),
        };
        let area = container(body)
            .padding(padding)
            .style(move |theme: &Theme| {
                container_style(Catalog::group_background(theme, color, placement))
            });
        match placement {
            Placement::Top => area.height(Length::Fill).into(),
            Placement::Left => area.width(Length::Fill).into(),
        }
    }

    /// Chevron, colour dot, name (or the app's editor), `locked` and the
    /// dissolve button. The whole header is a press and hover target.
    #[allow(clippy::too_many_arguments)]
    fn group_header(
        &self,
        section: &S,
        id: &G,
        label: Cow<'a, str>,
        color: Color,
        locked: bool,
        collapsed: bool,
        editor: Option<Element<'a, Message, Theme>>,
    ) -> Element<'a, Message, Theme> {
        let handlers = self.handlers;
        let placement = self.placement;
        let marked = marks_group(self.marker.as_ref(), section, id);
        let text_color =
            move |theme: &Theme| Catalog::group_header(theme, color, false, placement).text;
        let press = (handlers.on_press_group)(section.clone(), id.clone());

        let mut content = Row::new().spacing(4).align_y(Alignment::Center);
        content = content.push(
            flat_button(
                text(chevron(collapsed)).size(12).line_height(1.0),
                text_color,
            )
            .padding([1, 3])
            .on_press((handlers.on_toggle_group)(section.clone(), id.clone())),
        );
        if !locked {
            let dot = Color { a: 1.0, ..color };
            content = content.push(
                flat_button(
                    container(space().width(10).height(10)).style(move |_: &Theme| {
                        container::Style {
                            background: Some(Background::Color(dot)),
                            border: border::rounded(5),
                            ..container::Style::default()
                        }
                    }),
                    text_color,
                )
                .padding(2)
                .on_press((handlers.on_cycle_color)(section.clone(), id.clone())),
            );
        }
        content = content.push(match editor {
            Some(editor) => container(editor).width(self.fill_left()).into(),
            None => Element::from(
                mouse_area(
                    container(text(shorten(label)).size(12).wrapping(Wrapping::None))
                        .width(self.fill_left())
                        .clip(true),
                )
                .on_press(press.clone())
                .on_double_click((handlers.on_rename_group)(section.clone(), id.clone())),
            ),
        });
        if locked {
            content = content.push(
                container(text("locked").size(10).wrapping(Wrapping::None)).style(
                    move |theme: &Theme| container::Style {
                        text_color: Some(Color {
                            a: 0.6,
                            ..text_color(theme)
                        }),
                        ..container::Style::default()
                    },
                ),
            );
        } else {
            content = content.push(
                flat_button(text("\u{00D7}").size(14).line_height(1.0), text_color)
                    .padding([1, 4])
                    .on_press((handlers.on_dissolve_group)(section.clone(), id.clone())),
            );
        }

        mouse_area(self.header(content).style(move |theme: &Theme| {
            container_style(Catalog::group_header(theme, color, marked, placement))
        }))
        .on_press(press)
        .on_enter((handlers.on_hover)(Target::GroupEnd(
            section.clone(),
            id.clone(),
        )))
        .into()
    }

    /// One tab: the accent dot, the label and the close button on one shape
    /// that reports the press itself, so a drag can start from it. Hovering
    /// its leading half targets `before`, its trailing half `after`.
    fn tab(
        &self,
        tab: Tab<'a, Id>,
        before: Target<G, S>,
        after: Target<G, S>,
    ) -> Element<'a, Message, Theme> {
        let placement = self.placement;
        let selected = self.active.as_ref() == Some(&tab.id);
        let fill = placement == Placement::Left;

        let mut content = Row::new().spacing(6).align_y(Alignment::Center);
        if let Some(accent) = tab.accent {
            content = content.push(container(space().width(8).height(8)).style(
                move |_: &Theme| container::Style {
                    background: Some(Background::Color(accent)),
                    border: border::rounded(4),
                    ..container::Style::default()
                },
            ));
        }
        let editor = {
            let mut renaming = self.renaming.borrow_mut();
            match renaming.take() {
                Some((id, editor)) if id == tab.id => Some(editor),
                other => {
                    *renaming = other;
                    None
                }
            }
        };
        let width = if fill { Length::Fill } else { Length::Shrink };
        content = content.push(match editor {
            Some(editor) => container(editor).width(width),
            None => container(text(shorten(tab.label)).size(13).wrapping(Wrapping::None))
                .width(width)
                .clip(true),
        });
        if tab.closable {
            // One class per tab: resolving it per draw would allocate every
            // frame.
            let close_class: <Theme as Catalog>::Class<'a> = <Theme as Catalog>::default();
            content = content.push(
                flat_button(
                    text("\u{00D7}").size(14).line_height(1.0),
                    move |theme: &Theme| {
                        Catalog::style(
                            theme,
                            &close_class,
                            Status {
                                selected,
                                hovered: false,
                                pressed: false,
                                placement,
                            },
                        )
                        .text
                    },
                )
                .padding([1, 4])
                .on_press((self.handlers.on_close)(tab.id.clone())),
            );
        }

        let body = match placement {
            Placement::Top => container(content)
                .height(Length::Fill)
                .padding([0, 10])
                .align_y(Alignment::Center),
            Placement::Left => container(content)
                .width(Length::Fill)
                .padding([5, 8])
                .align_y(Alignment::Center),
        };

        Pressable {
            content: body.into(),
            on_press: (self.handlers.on_press_tab)(tab.id.clone()),
            on_double: (self.handlers.on_rename_tab)(tab.id),
            on_hover: [
                (self.handlers.on_hover)(before),
                (self.handlers.on_hover)(after),
            ],
            class: <Theme as Catalog>::default(),
            selected,
            placement,
            status: None,
        }
        .into()
    }

    /// The gap before an item, filled with the marker colour when marked.
    fn slot(&self, marked: bool) -> Element<'a, Message, Theme> {
        let (width, height) = match self.placement {
            Placement::Top => (Length::Fixed(SLOT), Length::Fill),
            Placement::Left => (Length::Fill, Length::Fixed(SLOT)),
        };
        container(space())
            .width(width)
            .height(height)
            .style(move |theme: &Theme| container::Style {
                background: marked.then(|| Background::Color(Catalog::marker(theme))),
                ..container::Style::default()
            })
            .into()
    }

    /// The drop zone before a root group, filled with the marker colour
    /// when marked.
    fn entry_zone(&self, target: Target<G, S>, marked: bool) -> Element<'a, Message, Theme> {
        let (width, height) = match self.placement {
            Placement::Top => (Length::Fixed(GROUP_ENTRY), Length::Fill),
            Placement::Left => (Length::Fill, Length::Fixed(GROUP_ENTRY)),
        };
        mouse_area(
            container(space())
                .width(width)
                .height(height)
                .style(move |theme: &Theme| container::Style {
                    background: marked.then(|| Background::Color(Catalog::marker(theme))),
                    ..container::Style::default()
                }),
        )
        .on_enter((self.handlers.on_hover)(target))
        .into()
    }

    /// The drop zone after the last item of a container.
    fn end_zone(&self, target: Target<G, S>, marked: bool) -> Element<'a, Message, Theme> {
        let (width, height) = match self.placement {
            Placement::Top => (Length::Fixed(END_ZONE), Length::Fill),
            Placement::Left => (Length::Fill, Length::Fixed(END_ZONE)),
        };
        mouse_area(
            container(space())
                .width(width)
                .height(height)
                .style(move |theme: &Theme| container::Style {
                    background: marked.then(|| {
                        Background::Color(Color {
                            a: 0.35,
                            ..Catalog::marker(theme)
                        })
                    }),
                    border: border::rounded(3),
                    ..container::Style::default()
                }),
        )
        .on_enter((self.handlers.on_hover)(target))
        .into()
    }

    /// The frame of a section or group header: as tall as the bar on top,
    /// as wide as the column at the left.
    fn header(
        &self,
        content: impl Into<Element<'a, Message, Theme>>,
    ) -> container::Container<'a, Message, Theme> {
        let header = container(content).align_y(Alignment::Center);
        match self.placement {
            Placement::Top => header.height(Length::Fill).padding([0, 4]),
            Placement::Left => header.width(Length::Fill).padding([2, 4]),
        }
    }

    fn fill_left(&self) -> Length {
        match self.placement {
            Placement::Top => Length::Shrink,
            Placement::Left => Length::Fill,
        }
    }
}

/// A borderless button in `text_color`, with a faint background on hover.
fn flat_button<'a, Message, Theme>(
    content: impl Into<Element<'a, Message, Theme>>,
    text_color: impl Fn(&Theme) -> Color + 'a,
) -> Button<'a, Message, Theme>
where
    Message: Clone + 'a,
    Theme: button::Catalog + 'a,
    <Theme as button::Catalog>::Class<'a>: From<button::StyleFn<'a, Theme>>,
{
    button(content).style(move |theme: &Theme, status| {
        let text = text_color(theme);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: hovered.then_some(Background::Color(Color { a: 0.15, ..text })),
            text_color: text,
            border: border::rounded(3),
            ..button::Style::default()
        }
    })
}

fn container_style(style: Style) -> container::Style {
    container::Style {
        background: style.background,
        text_color: Some(style.text),
        border: style.border,
        ..container::Style::default()
    }
}

/// `label` as shown: whole up to [`MAX_LABEL_CHARS`], else its first
/// characters and an ellipsis, [`MAX_LABEL_CHARS`] in all.
fn shorten(label: Cow<'_, str>) -> Cow<'_, str> {
    if label.chars().nth(MAX_LABEL_CHARS).is_none() {
        return label;
    }
    let cut = label
        .char_indices()
        .nth(MAX_LABEL_CHARS - 1)
        .map_or(label.len(), |(at, _)| at);
    let mut short = label[..cut].trim_end().to_owned();
    short.push('\u{2026}');
    Cow::Owned(short)
}

/// A tab body. It reports the press when the button goes down, not on
/// release as a button does, so the app can tell a click from a drag; and it
/// draws the tab look with hover and press status, which a `mouse_area`
/// around a `container` cannot. A press a child captured (the close button)
/// is not reported.
struct Pressable<'a, Message, Theme, Renderer>
where
    Theme: Catalog,
{
    content: Element<'a, Message, Theme, Renderer>,
    on_press: Message,
    /// Published after `on_press` when the press completes a double click.
    on_double: Message,
    /// Published when the cursor enters the leading (`[0]`) or the trailing
    /// (`[1]`) half, or crosses from one to the other.
    on_hover: [Message; 2],
    class: <Theme as Catalog>::Class<'a>,
    selected: bool,
    placement: Placement,
    /// Hover and press as of the last redraw; a change requests another.
    status: Option<(bool, bool)>,
}

#[derive(Default)]
struct PressState {
    hovered: bool,
    /// The half last published: `Some(true)` is the trailing one.
    half: Option<bool>,
    pressed: bool,
    last_click: Option<mouse::Click>,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Pressable<'_, Message, Theme, Renderer>
where
    Message: Clone,
    Theme: Catalog,
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PressState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PressState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let state = tree.state.downcast_mut::<PressState>();
        let hovered = cursor.is_over(layout.bounds());
        let half = cursor
            .position()
            .filter(|_| hovered)
            .map(|at| trailing_half(self.placement, layout.bounds(), at));
        if let Some(trailing) = half
            && state.half != half
        {
            shell.publish(self.on_hover[usize::from(trailing)].clone());
        }
        state.half = half;
        state.hovered = hovered;

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. })
                if hovered && !shell.is_event_captured() =>
            {
                state.pressed = true;
                shell.publish(self.on_press.clone());
                if let Some(position) = cursor.position() {
                    let click = mouse::Click::new(position, mouse::Button::Left, state.last_click);
                    state.last_click = Some(click);
                    if click.kind() == mouse::click::Kind::Double {
                        shell.publish(self.on_double.clone());
                    }
                }
                shell.capture_event();
            }
            // The release is left to the app, which ends a drag on it
            // wherever the cursor is.
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerLifted { .. } | touch::Event::FingerLost { .. }) => {
                state.pressed = false;
            }
            _ => {}
        }

        let current = (state.hovered, state.pressed);
        if let Event::Window(window::Event::RedrawRequested(_)) = event {
            self.status = Some(current);
        } else if self.status.is_some_and(|status| status != current) {
            shell.request_redraw();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        match self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        ) {
            mouse::Interaction::None if cursor.is_over(layout.bounds()) => {
                mouse::Interaction::Pointer
            }
            interaction => interaction,
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let (hovered, pressed) = self.status.unwrap_or_default();
        let style = theme.style(
            &self.class,
            Status {
                selected: self.selected,
                hovered,
                pressed,
                placement: self.placement,
            },
        );
        if style.background.is_some() || style.border.width > 0.0 {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: style.border,
                    ..renderer::Quad::default()
                },
                style
                    .background
                    .unwrap_or(Background::Color(Color::TRANSPARENT)),
            );
        }
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            &renderer::Style {
                text_color: style.text,
            },
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<Pressable<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: Catalog + 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(pressable: Pressable<'a, Message, Theme, Renderer>) -> Self {
        Element::new(pressable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_label_is_cut_to_the_limit_with_an_ellipsis() {
        let exact = "x".repeat(MAX_LABEL_CHARS);
        assert!(matches!(shorten(Cow::Borrowed(&exact)), Cow::Borrowed(_)));

        // Multi-byte characters are counted as characters, never split.
        let long = "\u{00e4}".repeat(MAX_LABEL_CHARS + 1);
        let short = shorten(Cow::Borrowed(&long));
        assert_eq!(short.chars().count(), MAX_LABEL_CHARS);
        assert!(short.ends_with('\u{2026}'));
    }

    #[test]
    fn a_before_marker_marks_only_its_own_container_and_index() {
        let root = Target::<u8, u8>::Before {
            section: 1,
            group: None,
            index: 2,
        };
        assert!(marks_slot(Some(&root), &1, None, 2));
        assert!(!marks_slot(Some(&root), &1, None, 1));
        assert!(!marks_slot(Some(&root), &2, None, 2));
        assert!(!marks_slot(Some(&root), &1, Some(&7), 2));

        let grouped = Target::<u8, u8>::Before {
            section: 1,
            group: Some(7),
            index: 0,
        };
        assert!(marks_slot(Some(&grouped), &1, Some(&7), 0));
        assert!(!marks_slot(Some(&grouped), &1, Some(&8), 0));
        assert!(!marks_slot(Some(&grouped), &1, None, 0));
        assert!(!marks_slot::<u8, u8>(None, &1, None, 0));
    }

    #[test]
    fn end_markers_mark_their_header_or_zone_and_no_slot() {
        let group_end = Target::<u8, u8>::GroupEnd(1, 7);
        assert!(marks_group(Some(&group_end), &1, &7));
        assert!(!marks_group(Some(&group_end), &2, &7));
        assert!(!marks_group(Some(&group_end), &1, &8));
        assert!(!marks_section_end(Some(&group_end), &1));
        assert!(!marks_slot(Some(&group_end), &1, Some(&7), 0));

        let section_end = Target::<u8, u8>::SectionEnd(1);
        assert!(marks_section_end(Some(&section_end), &1));
        assert!(!marks_section_end(Some(&section_end), &2));
        assert!(!marks_group(Some(&section_end), &1, &7));
        assert!(!marks_slot(Some(&section_end), &1, None, 0));
    }

    #[test]
    fn the_trailing_half_follows_the_bar_direction() {
        let bounds = Rectangle::new(Point::new(10.0, 20.0), Size::new(40.0, 30.0));
        let (cx, cy) = (bounds.center_x(), bounds.center_y());
        assert!(!trailing_half(
            Placement::Top,
            bounds,
            Point::new(cx - 1.0, cy + 5.0)
        ));
        assert!(trailing_half(
            Placement::Top,
            bounds,
            Point::new(cx + 1.0, cy - 5.0)
        ));
        assert!(!trailing_half(
            Placement::Left,
            bounds,
            Point::new(cx + 5.0, cy - 1.0)
        ));
        assert!(trailing_half(
            Placement::Left,
            bounds,
            Point::new(cx - 5.0, cy + 1.0)
        ));
    }
}
