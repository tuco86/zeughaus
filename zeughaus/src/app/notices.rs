//! The notice center, after griasdi's: one box per notice, newest on top, in a
//! panel that slides in from the right. A slim pull tab at the bottom right
//! opens it and counts the notices in the colour of the most severe; a click
//! outside the panel, the tab or Escape closes it. Each box shows its
//! severity, title, body, age and, when it stays until resolved, a pin.

use std::time::SystemTime;

use iced::widget::{
    Space, Stack, button, column, container, float, mouse_area, opaque, row, scrollable, text,
    tooltip,
};
use iced::{Alignment, Background, Border, Element, Length, Padding, Vector};
use zeughaus_theme::Theme;

use super::App;
use crate::message::Message;
use crate::notices::{self, Notice, Severity};
use crate::style::{self, Tokens};

/// Width of the open center.
const CENTER_WIDTH: f32 = 340.0;
/// Width of the pull tab at the window's right edge.
const TAB_WIDTH: f32 = 14.0;
const TAB_HEIGHT: f32 = 40.0;
/// Distance of the tab from the window's bottom edge, clear of the corner
/// resize grip.
const TAB_BOTTOM: f32 = 20.0;

/// The pin of a notice that stays until it is dismissed or resolved.
const PIN: char = '\u{eba0}';
/// The dismiss button's cross.
const CLOSE: char = '\u{ea76}';

impl App {
    /// Raises a notice now; see [`notices::Notices::raise`].
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(super) fn raise(
        &mut self,
        key: Option<String>,
        severity: Severity,
        title: impl Into<String>,
        body: impl Into<String>,
        sticky: bool,
        open: Option<crate::message::NoticeTarget>,
    ) -> u64 {
        self.notices
            .raise(key, severity, title, body, sticky, open, SystemTime::now())
    }

    /// Closes the notice center once nothing is left in it.
    pub(super) fn close_empty_notice_center(&mut self) {
        if self.notices.is_empty() {
            self.notice_center.go_mut(false, iced::time::Instant::now());
        }
    }
}

/// `content` with `words` in a bubble to its left, styled as the tab bar's
/// tooltips are.
fn tip<'a>(
    content: impl Into<Element<'a, Message, Theme>>,
    words: &'a str,
) -> Element<'a, Message, Theme> {
    let bubble = container(text(words).size(12))
        .padding([3, 6])
        .style(|theme: &Theme| {
            let style = iced_tabs::Catalog::tooltip(theme);
            container::Style {
                background: style.background,
                text_color: Some(style.text),
                border: style.border,
                ..container::Style::default()
            }
        });
    tooltip(content, bubble, tooltip::Position::Left)
        .gap(4)
        .into()
}

/// The pull tab: the number of notices, filled in the colour of the most
/// severe.
fn tab<'a>(app: &'a App, open: bool) -> Element<'a, Message, Theme> {
    let worst = app.notices.worst().unwrap_or(Severity::Info);
    let count = container(
        text(app.notices.len().to_string())
            .size(10)
            .font(style::BOLD),
    )
    .center_x(Length::Fill)
    .center_y(Length::Fill);
    let tab = button(count)
        .width(TAB_WIDTH)
        .height(TAB_HEIGHT)
        .padding(0)
        .style(style::notice_tab(worst))
        .on_press(Message::ToggleNoticeCenter);
    tip(tab, if open { "Hide notices" } else { "Notices" })
}

/// The open panel: the header with "Clear all", then the boxes, newest on
/// top.
fn panel<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message, Theme> {
    let now = SystemTime::now();
    let clear = button(text("Clear all").size(12))
        .padding([3, 10])
        .style(style::secondary_button(t))
        .on_press(Message::DismissAllNotices);
    let header = row![
        text("Notices").size(14).font(style::BOLD),
        Space::new().width(Length::Fill),
        clear,
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let boxes = column(
        app.notices
            .iter()
            .rev()
            .map(|notice| notice_box(app, t, notice, now)),
    )
    .spacing(6);
    opaque(
        container(column![header, scrollable(boxes).height(Length::Fill)].spacing(10))
            .padding(12)
            .width(CENTER_WIDTH)
            .height(Length::Fill)
            .style(style::notice_center(t, app.window_radius())),
    )
}

/// The notice center layer: only the pull tab while closed, the tab glued
/// to the sliding panel while open or animating; `None` with nothing to show.
pub(super) fn center(app: &App) -> Option<Element<'_, Message, Theme>> {
    let now = iced::time::Instant::now();
    let animating = app.notice_center.is_animating(now);
    let open = app.notice_center.value();
    if app.notices.is_empty() && !animating {
        return None;
    }
    let t = Tokens::of(&app.theme);
    let lift = Padding {
        bottom: TAB_BOTTOM,
        ..Padding::ZERO
    };
    if !open && !animating {
        return Some(
            container(tab(app, false))
                .padding(lift)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_right(Length::Fill)
                .align_bottom(Length::Fill)
                .into(),
        );
    }
    let p: f32 = app.notice_center.interpolate(0.0, 1.0, now);
    let slot: Element<'_, Message, Theme> = if app.notices.is_empty() {
        // The last notice is gone while the panel slides out.
        Space::new().width(TAB_WIDTH).into()
    } else {
        tab(app, open)
    };
    let unit = row![
        container(slot)
            .padding(lift)
            .height(Length::Fill)
            .align_bottom(Length::Fill),
        panel(app, &t),
    ]
    .height(Length::Fill);
    let placed =
        container(float(unit).translate(move |_, _| Vector::new((1.0 - p) * CENTER_WIDTH, 0.0)))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_right(Length::Fill);
    if !open {
        // Sliding out: nothing to dismiss any more.
        return Some(placed.into());
    }
    // A click anywhere outside the panel and the tab (both capture their
    // own) closes it.
    let backdrop = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::CloseNoticeCenter);
    Some(Stack::with_children([backdrop.into(), placed.into()]).into())
}

/// One box: the severity dot, the title over the body, the age, the pin of
/// a sticky notice and the small x. With a target the title is a button.
fn notice_box<'a>(
    app: &'a App,
    t: &Tokens,
    notice: &'a Notice,
    now: SystemTime,
) -> Element<'a, Message, Theme> {
    let muted = app.theme.muted();
    let color = style::severity_color(&app.theme, notice.severity);
    let dot = container(Space::new())
        .width(10)
        .height(10)
        .style(move |_: &Theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                radius: 5.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });
    let title = text(notice.title.as_str()).size(13).font(style::BOLD);
    let title: Element<'a, Message, Theme> = if notice.open.is_some() {
        button(title)
            .padding([1, 4])
            .width(Length::Fill)
            .style(style::channel_button(t, false))
            .on_press(Message::OpenNotice(notice.id))
            .into()
    } else {
        title.width(Length::Fill).into()
    };
    let mut words = column![title].spacing(2).width(Length::Fill);
    if !notice.body.is_empty() {
        words = words.push(text(notice.body.as_str()).size(12).color(muted));
    }
    let close = button(
        text(CLOSE.to_string())
            .size(12)
            .line_height(iced::widget::text::LineHeight::Absolute(12.0.into())),
    )
    .padding(4)
    .style(style::channel_button(t, false))
    .on_press(Message::DismissNotice(notice.id));
    let mut head = row![
        container(dot).padding([4, 0]),
        words,
        text(notices::age(notice.raised, now)).size(12).color(muted),
    ]
    .spacing(8)
    .align_y(Alignment::Start);
    if notice.sticky {
        head = head.push(text(PIN.to_string()).size(12).color(muted));
    }
    head = head.push(tip(close, "Dismiss"));
    container(head)
        .padding([8, 10])
        .width(Length::Fill)
        .style(style::card(t))
        .into()
}
