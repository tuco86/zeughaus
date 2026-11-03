//! The places Back and Forward walk: what the window shows (a runner's tab
//! or a view of the CI page, with the ticket popout over it) named by ids
//! that survive a restart, recorded after every message into
//! [`crate::history::History`], and the popover that draws that tree under
//! the titlebar's History button.
//!
//! A CI view's id is minted per process, so a place names a channel by its
//! repository and name and is resolved when it is shown.

use std::time::{Instant, SystemTime};

use iced::widget::{
    Space, Stack, button, canvas, column, container, mouse_area, opaque, row, text,
};
use iced::{Alignment, Color, Element, Length, Padding, Point, Rectangle, Task, mouse};
use iced_tabs::scroll;
use serde::{Deserialize, Serialize};
use zeughaus_mux::TabId;
use zeughaus_theme::Theme;

use super::{App, PageView, TITLEBAR_HEIGHT, ci};
use crate::history::{History, Mark, Row};
use crate::message::{CiMsg, Message, NavMsg};
use crate::notices;
use crate::style::{self, Tokens};
use crate::workspace::{RunnerKey, TabRef};

/// The titlebar's arrows and the tree's button (codicons).
pub(super) const GLYPH_BACK: char = '\u{ea9b}';
pub(super) const GLYPH_FORWARD: char = '\u{ea9c}';
pub(super) const GLYPH_HISTORY: char = '\u{ea82}';
/// The kinds of place in the tree: the Overview and a Board.
const GLYPH_OVERVIEW: char = '\u{eacd}';
const GLYPH_BOARD: char = '\u{eb30}';

/// The tree's list, which the arrow keys scroll.
const HISTORY_LIST: &str = "history-list";
/// A row of the tree, and the width of one lane of its graph.
const ROW: f32 = 26.0;
const LANE: f32 = 14.0;
/// The most rows the tree shows before it scrolls.
const ROWS_SHOWN: f32 = 14.0;
const PANEL_WIDTH: f32 = 400.0;
/// The longest label the tree keeps, in characters.
const LABEL_MAX: usize = 48;

/// What the front of the window shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum View {
    Tab {
        #[serde(with = "key_text")]
        runner: RunnerKey,
        tab: u64,
    },
    Overview,
    Board {
        origin: String,
    },
    Channel {
        #[serde(with = "key_text")]
        ci: RunnerKey,
        repo: String,
        channel: String,
    },
    Job {
        #[serde(with = "key_text")]
        ci: RunnerKey,
        repo: String,
        channel: String,
        number: u64,
        job: String,
    },
}

/// The ticket the popout shows over a view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TicketPlace {
    #[serde(with = "key_text")]
    ci: RunnerKey,
    repo: String,
    number: u64,
}

/// One place of the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Place {
    view: View,
    ticket: Option<TicketPlace>,
}

/// A runner key as its text: `RunnerKey` stays serde-free, because serde is
/// a native-only dependency.
mod key_text {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::workspace::RunnerKey;

    pub fn serialize<S: Serializer>(key: &RunnerKey, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(key.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RunnerKey, D::Error> {
        String::deserialize(deserializer).map(|text| RunnerKey::new(&text))
    }
}

impl App {
    /// Whether the window shows `place` now. Runs after every message, so it
    /// allocates nothing.
    fn shows(&self, place: &Place) -> bool {
        let channel_is = |ci: &RunnerKey, view: u64, repo: &str, channel: &str| {
            self.ci.get(ci).and_then(|client| client.view_channel(view)) == Some((repo, channel))
        };
        let view = match (&place.view, self.page.open, &self.page.view) {
            (View::Tab { runner, tab }, false, _) => self
                .workspace
                .active_tab()
                .is_some_and(|active| active.runner == *runner && active.tab.0 == *tab),
            (View::Overview, true, PageView::Overview) => true,
            (View::Board { origin }, true, PageView::Board(shown)) => origin == shown,
            (View::Channel { ci, repo, channel }, true, PageView::Channel(shown, view)) => {
                ci == shown && channel_is(ci, *view, repo, channel)
            }
            (
                View::Job {
                    ci,
                    repo,
                    channel,
                    number,
                    job,
                },
                true,
                PageView::Job(shown, open),
            ) => {
                ci == shown
                    && open.number == *number
                    && open.job == *job
                    && channel_is(ci, open.view, repo, channel)
            }
            _ => false,
        };
        view && match (&place.ticket, self.popout_ticket()) {
            (None, None) => true,
            (Some(ticket), Some((ci, repo, number))) => {
                ticket.ci == *ci && ticket.repo == repo && ticket.number == number
            }
            _ => false,
        }
    }

    /// What the window shows now, as a place; `None` while that is nothing
    /// the history can come back to.
    fn place(&self) -> Option<Place> {
        let view = if self.page.open {
            match &self.page.view {
                PageView::Overview => View::Overview,
                PageView::Board(origin) => View::Board {
                    origin: origin.clone(),
                },
                PageView::Channel(ci, view) => {
                    let (repo, channel) = self.ci.get(ci)?.view_channel(*view)?;
                    View::Channel {
                        ci: ci.clone(),
                        repo: repo.to_owned(),
                        channel: channel.to_owned(),
                    }
                }
                PageView::Job(ci, job) => {
                    let (repo, channel) = self.ci.get(ci)?.view_channel(job.view)?;
                    View::Job {
                        ci: ci.clone(),
                        repo: repo.to_owned(),
                        channel: channel.to_owned(),
                        number: job.number,
                        job: job.job.clone(),
                    }
                }
            }
        } else {
            let tab = self.workspace.active_tab()?;
            // The Projects section's tab ids are this process's own.
            if tab.runner.as_str() == RunnerKey::PROJECTS {
                return None;
            }
            View::Tab {
                runner: tab.runner.clone(),
                tab: tab.tab.0,
            }
        };
        let ticket = self.popout_ticket().map(|(ci, repo, number)| TicketPlace {
            ci: ci.clone(),
            repo: repo.to_owned(),
            number,
        });
        Some(Place { view, ticket })
    }

    /// Whether `place` can be shown: its tab is still there, its CI still
    /// lists its channel and its ticket's repository.
    pub(super) fn place_available(&self, place: &Place) -> bool {
        let view = match &place.view {
            View::Tab { runner, tab } => self
                .workspace
                .section(runner)
                .is_some_and(|section| section.snapshot.tabs().any(|t| t.id.0 == *tab)),
            View::Overview | View::Board { .. } => true,
            View::Channel {
                ci, repo, channel, ..
            }
            | View::Job {
                ci, repo, channel, ..
            } => self
                .ci
                .get(ci)
                .is_some_and(|client| client.channel_view(repo, channel).is_some()),
        };
        view && place.ticket.as_ref().is_none_or(|ticket| {
            self.ci
                .get(&ticket.ci)
                .is_some_and(|client| client.plans_view_of(&ticket.repo).is_some())
        })
    }

    /// What the tree calls `place` now.
    fn place_label(&self, place: &Place) -> String {
        let mut label = match &place.view {
            View::Tab { runner, tab } => self
                .workspace
                .section(runner)
                .and_then(|section| section.snapshot.tabs().find(|t| t.id.0 == *tab))
                .map_or_else(|| "Closed tab".to_owned(), |t| t.title.clone()),
            View::Overview => "Overview".to_owned(),
            View::Board { origin } => {
                let name = self
                    .projects
                    .groups
                    .iter()
                    .find(|group| group.origin == *origin)
                    .map_or_else(
                        || origin.rsplit('/').next().unwrap_or(origin).to_owned(),
                        |group| group.name.clone(),
                    );
                format!("{name} Board")
            }
            View::Channel { repo, channel, .. } => format!("{channel} \u{b7} {repo}"),
            View::Job { number, job, .. } => format!("#{number} {job}"),
        };
        if let Some(ticket) = &place.ticket {
            let title = self
                .ticket_title(&ticket.ci, &ticket.repo, ticket.number)
                .unwrap_or_default();
            label.push_str(&format!(" \u{203a} #{} {title}", ticket.number));
        }
        cut(label.trim_end())
    }

    /// Brings `place` to the front: its tab or CI view, then its ticket.
    pub(super) fn show_place(&mut self, place: &Place) -> Task<Message> {
        let shown = match &place.view {
            View::Tab { runner, tab } => {
                self.page.open = false;
                self.workspace.activate(TabRef {
                    runner: runner.clone(),
                    tab: TabId(*tab),
                });
                Task::none()
            }
            View::Overview => self.apply_page(Message::PageShow(PageView::Overview)),
            View::Board { origin } => {
                self.apply_page(Message::PageShow(PageView::Board(origin.clone())))
            }
            View::Channel { ci, repo, channel } => {
                match self.ci.get(ci).and_then(|c| c.channel_view(repo, channel)) {
                    Some(view) => {
                        self.apply_page(Message::PageShow(PageView::Channel(ci.clone(), view)))
                    }
                    None => Task::none(),
                }
            }
            View::Job {
                ci,
                repo,
                channel,
                number,
                job,
            } => match self.ci.get(ci).and_then(|c| c.channel_view(repo, channel)) {
                // Opens the transcript again when its tab was closed.
                Some(view) => self.apply_ci(
                    ci.clone(),
                    CiMsg::OpenJob {
                        view,
                        number: *number,
                        job: job.clone(),
                    },
                ),
                None => Task::none(),
            },
        };
        let ticket = match &place.ticket {
            Some(ticket) => self.open_ticket(&ticket.ci, &ticket.repo, ticket.number),
            None => {
                self.ticket = None;
                Task::none()
            }
        };
        Task::batch([shown, ticket])
    }

    /// Records what the window shows, when it moved since the last time.
    pub(super) fn record_place(&mut self) {
        if self.restoring_place() {
            return;
        }
        if self
            .history
            .current()
            .is_some_and(|place| self.shows(place))
        {
            return;
        }
        let Some(place) = self.place() else {
            return;
        };
        let label = self.place_label(&place);
        self.history.visit(place, label, Instant::now());
    }

    /// What Back, Forward and the tree do.
    pub(super) fn apply_nav(&mut self, msg: NavMsg) -> Task<Message> {
        let now = Instant::now();
        match msg {
            NavMsg::Back | NavMsg::Forward => {
                // Out for the walk, so availability can look at the rest.
                let mut history = std::mem::take(&mut self.history);
                let available = |place: &Place| self.place_available(place);
                let place = if msg == NavMsg::Back {
                    history.back(available, now)
                } else {
                    history.forward(available, now)
                };
                self.history = history;
                match place {
                    Some(place) => self.show_place(&place),
                    None => Task::none(),
                }
            }
            NavMsg::ToggleTree => {
                self.history_open = !self.history_open;
                if self.history_open {
                    self.palette_close();
                    self.history_cursor = self
                        .history
                        .rows()
                        .iter()
                        .position(|row| row.mark == Mark::Current)
                        .unwrap_or(0);
                }
                Task::none()
            }
            NavMsg::CloseTree => {
                self.history_open = false;
                Task::none()
            }
            NavMsg::Jump(id) => {
                let available = self
                    .history
                    .place(id)
                    .is_some_and(|(place, ..)| self.place_available(place));
                if !available {
                    return Task::none();
                }
                self.history_open = false;
                match self.history.jump(id, now) {
                    Some(place) => self.show_place(&place),
                    None => Task::none(),
                }
            }
        }
    }

    /// The arrow keys in the open tree: its cursor, kept in view.
    pub(super) fn history_step(&mut self, step: isize) -> Task<Message> {
        let count = self.history.len();
        if count == 0 {
            return Task::none();
        }
        self.history_cursor =
            (self.history_cursor as isize + step).clamp(0, count as isize - 1) as usize;
        crate::palette::reveal(HISTORY_LIST, self.history_cursor, count)
    }

    /// Enter in the open tree: to the row under its cursor.
    pub(super) fn history_confirm(&mut self) -> Task<Message> {
        match self.history.rows().get(self.history_cursor) {
            Some(row) => self.apply_nav(NavMsg::Jump(row.id)),
            None => Task::none(),
        }
    }

    /// The tree under the History button, newest place on top; `None`
    /// while it is shut.
    pub(super) fn history_view(&self) -> Option<Element<'_, Message, Theme>> {
        if !self.history_open {
            return None;
        }
        let t = Tokens::of(&self.theme);
        let muted = self.theme.muted();
        let rows = self.history.rows();
        let lanes = History::<Place>::lanes(&rows);
        let now = SystemTime::now();
        let header = row![
            text("History").size(13).font(style::BOLD),
            Space::new().width(Length::Fill),
            text(format!("{} places", rows.len())).size(12).color(muted),
        ]
        .align_y(Alignment::Center);
        let list = column(rows.into_iter().enumerate().filter_map(|(index, row)| {
            let (place, stored, visited) = self.history.place(row.id)?;
            let gone = !self.place_available(place);
            let id = row.id;
            let current = row.mark == Mark::Current;
            let rail = canvas(Rail { row, gone })
                .width(lanes as f32 * LANE + 6.0)
                .height(ROW);
            let label = if gone {
                format!("{stored} (closed)")
            } else {
                self.place_label(place)
            };
            let mut label = text(label).size(13).width(Length::Fill);
            if current {
                label = label.font(style::BOLD);
            }
            if gone {
                label = label.color(muted);
            }
            let kind = container(text(self.place_glyph(place).to_string()).size(14))
                .width(18)
                .center_x(18);
            let mut line = row![rail, kind, label]
                .spacing(4)
                .align_y(Alignment::Center);
            if let View::Tab { runner, .. } = &place.view
                && let Some((glyph, _)) = self.host_of(runner)
            {
                line = line.push(text(glyph.to_string()).size(12).color(muted));
            }
            let raised = now.checked_sub(visited.elapsed()).unwrap_or(now);
            line = line.push(text(notices::age(raised, now)).size(12).color(muted));
            let entry = button(line)
                .height(ROW)
                .padding([0, 4])
                .width(Length::Fill)
                .style(style::channel_button(&t, index == self.history_cursor))
                .on_press_maybe((!gone).then_some(Message::Nav(NavMsg::Jump(id))));
            Some(entry.into())
        }));
        let list = container(scroll::vertical(list).id(HISTORY_LIST)).max_height(ROWS_SHOWN * ROW);
        let panel = container(column![header, list].spacing(6))
            .padding(8)
            .width(PANEL_WIDTH)
            .style(style::popover(&t));
        // Its right edge under the History button, the window's three
        // buttons to its right.
        let placed = container(opaque(panel))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_right(Length::Fill)
            .padding(Padding {
                top: 4.0,
                right: 3.0 * TITLEBAR_HEIGHT,
                ..Padding::ZERO
            });
        let backdrop = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
            .on_press(Message::Nav(NavMsg::CloseTree));
        Some(Stack::with_children([backdrop.into(), placed.into()]).into())
    }

    /// The glyph of a place's kind.
    fn place_glyph(&self, place: &Place) -> char {
        match &place.view {
            View::Tab { runner, tab } => {
                let tab = TabRef {
                    runner: runner.clone(),
                    tab: TabId(*tab),
                };
                if self.workspace.single_graph(&tab).is_some() {
                    super::GLYPH_GRAPH
                } else {
                    super::GLYPH_SHELL
                }
            }
            View::Overview => GLYPH_OVERVIEW,
            View::Board { .. } => GLYPH_BOARD,
            View::Channel { .. } => ci::glyph::BRANCH,
            View::Job { .. } => ci::glyph::LOG,
        }
    }
}

/// `label` cut to [`LABEL_MAX`] characters.
fn cut(label: &str) -> String {
    if label.chars().count() <= LABEL_MAX {
        return label.to_owned();
    }
    let kept: String = label.chars().take(LABEL_MAX - 1).collect();
    format!("{kept}\u{2026}")
}

/// One row's piece of the tree's graph: the lines through it and its dot.
struct Rail {
    row: Row,
    gone: bool,
}

impl canvas::Program<Message, Theme> for Rail {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let x = |lane: usize| 3.0 + LANE * (lane as f32 + 0.5);
        let centre = ROW / 2.0;
        let line = canvas::Stroke::default()
            .with_color(Color {
                a: 0.6,
                ..theme.muted()
            })
            .with_width(1.5);
        let lane = x(self.row.lane);
        let lines = canvas::Path::new(|p| {
            for &through in &self.row.through {
                p.move_to(Point::new(x(through), 0.0));
                p.line_to(Point::new(x(through), ROW));
            }
            if self.row.up {
                p.move_to(Point::new(lane, 0.0));
                p.line_to(Point::new(lane, centre));
            }
            if self.row.down {
                p.move_to(Point::new(lane, centre));
                p.line_to(Point::new(lane, ROW));
            }
            for &join in &self.row.joins {
                p.move_to(Point::new(x(join), 0.0));
                p.quadratic_curve_to(Point::new(x(join), centre), Point::new(lane, centre));
            }
        });
        frame.stroke(&lines, line);
        let dot = Point::new(lane, centre);
        let accent = theme.accent();
        let ring = |color: Color| canvas::Stroke::default().with_color(color).with_width(1.5);
        match self.row.mark {
            Mark::Current => {
                frame.fill(&canvas::Path::circle(dot, 4.5), accent);
                frame.stroke(&canvas::Path::circle(dot, 7.0), ring(accent));
            }
            Mark::Behind if !self.gone => {
                frame.fill(&canvas::Path::circle(dot, 3.5), theme.base().palette().text);
            }
            Mark::Ahead if !self.gone => {
                frame.stroke(&canvas::Path::circle(dot, 3.5), ring(accent));
            }
            _ => {
                frame.stroke(&canvas::Path::circle(dot, 3.5), ring(theme.muted()));
            }
        }
        vec![frame.into_geometry()]
    }
}
