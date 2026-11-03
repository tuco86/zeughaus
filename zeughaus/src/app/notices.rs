//! The notices, after griasdi's: one box per notice, newest on top, in a
//! popout over the status bar's attention chip. It opens while the pointer
//! is on the chip or on the popout itself and closes a moment after it
//! left both; "Notices / Show" in the palette pins it open until a click
//! outside or Escape. Each box shows its severity, title, body, age and,
//! when it stays until resolved, a pin.

// The browser editor raises no notices and draws no popout.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::time::SystemTime;

use iced::widget::{Space, button, column, container, row, text, tooltip};
#[cfg(not(target_arch = "wasm32"))]
use iced::widget::{Stack, mouse_area, opaque};
use iced::{Alignment, Background, Border, Element, Length};
#[cfg(not(target_arch = "wasm32"))]
use iced::{Padding, Task};
#[cfg(not(target_arch = "wasm32"))]
use iced_tabs::scroll;
use zeughaus_theme::Theme;

use super::App;
use crate::message::Message;
#[cfg(not(target_arch = "wasm32"))]
use crate::message::NoticeZone;
use crate::notices::{self, Notice, Severity};
use crate::style::{self, Tokens};

/// Width of the popout.
#[cfg(not(target_arch = "wasm32"))]
const POPOUT_WIDTH: f32 = 360.0;
/// The tallest the popout's list grows before it scrolls.
#[cfg(not(target_arch = "wasm32"))]
const POPOUT_MAX_HEIGHT: f32 = 420.0;
/// How long the popout stays after the pointer left the chip and the
/// popout: long enough to cross the gap between them.
#[cfg(not(target_arch = "wasm32"))]
const NOTICE_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// The pin of a notice that stays until it is dismissed or resolved.
const PIN: char = '\u{eba0}';
/// The dismiss button's cross.
const CLOSE: char = '\u{ea76}';

/// Where the pointer is, and whether the palette pinned the popout open.
/// Each change bumps `generation`, so a grace period that ended after the
/// pointer came back closes nothing.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub(super) struct NoticeHover {
    chip: bool,
    popout: bool,
    pinned: bool,
    generation: u64,
}

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

    /// Closes the notices once nothing is left in them.
    pub(super) fn close_empty_notices(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.notices.is_empty() {
            self.notice_popout = false;
            self.notice_hover.pinned = false;
        }
    }

    /// Shuts the popout, pinned or not.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn close_notices(&mut self) {
        self.notice_hover.pinned = false;
        self.notice_popout = false;
    }

    /// What the pointer, the chip and the palette do to the popout.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn apply_notices(&mut self, message: Message) -> Task<Message> {
        let hover = &mut self.notice_hover;
        match message {
            Message::NoticeHover(zone, inside) => {
                match zone {
                    NoticeZone::Chip => hover.chip = inside,
                    NoticeZone::Popout => hover.popout = inside,
                }
                hover.generation += 1;
                if hover.chip || hover.popout || hover.pinned {
                    self.notice_popout = !self.notices.is_empty() || hover.pinned;
                } else {
                    let generation = hover.generation;
                    return Task::perform(tokio::time::sleep(NOTICE_GRACE), move |()| {
                        Message::NoticeHoverEnd(generation)
                    });
                }
            }
            Message::NoticeHoverEnd(generation) => {
                if generation == hover.generation && !(hover.chip || hover.popout || hover.pinned) {
                    self.notice_popout = false;
                }
            }
            Message::ToggleNotices => {
                hover.pinned = !self.notice_popout;
                self.notice_popout = hover.pinned;
            }
            Message::CloseNotices => self.close_notices(),
            // The popout stays shut until the pointer enters the chip again.
            Message::AttentionChip => {
                *hover = NoticeHover {
                    generation: hover.generation + 1,
                    ..NoticeHover::default()
                };
                self.notice_popout = false;
                return self.apply_page(Message::TogglePage);
            }
            _ => {}
        }
        Task::none()
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

/// The popout: the header with "Clear all", then the boxes, newest on top,
/// right above the chip. `None` while it is shut.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn popout(app: &App) -> Option<Element<'_, Message, Theme>> {
    if !app.notice_popout {
        return None;
    }
    let t = Tokens::of(&app.theme);
    let now = SystemTime::now();
    let clear = button(text("Clear all").size(12))
        .padding([3, 10])
        .style(style::secondary_button(&t))
        .on_press_maybe((!app.notices.is_empty()).then_some(Message::DismissAllNotices));
    let header = row![
        text("Notices").size(14).font(style::BOLD),
        Space::new().width(Length::Fill),
        clear,
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let body: Element<'_, Message, Theme> = if app.notices.is_empty() {
        text("No notices").size(12).color(app.theme.muted()).into()
    } else {
        let boxes = column(
            app.notices
                .iter()
                .rev()
                .map(|notice| notice_box(app, &t, notice, now)),
        )
        .spacing(6);
        container(scroll::vertical(boxes))
            .max_height(POPOUT_MAX_HEIGHT)
            .into()
    };
    let panel = container(column![header, body].spacing(10))
        .padding(12)
        .width(POPOUT_WIDTH)
        .style(style::popover(&t));
    let panel = mouse_area(opaque(panel))
        .on_enter(Message::NoticeHover(NoticeZone::Popout, true))
        .on_exit(Message::NoticeHover(NoticeZone::Popout, false));
    let placed = container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_right(Length::Fill)
        .align_bottom(Length::Fill)
        .padding(Padding {
            right: super::STATUS_GAP,
            bottom: 4.0,
            ..Padding::ZERO
        });
    if !app.notice_hover.pinned {
        return Some(placed.into());
    }
    // Pinned: a click anywhere outside the popout closes it.
    let backdrop = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::CloseNotices);
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
