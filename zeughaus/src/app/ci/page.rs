//! The CI page: a full-window view the titlebar chip opens over the
//! workspace. A sidebar lists what needs the user and what is working, then
//! each CI with its repositories (their channels and plans), its sessions
//! and its runners; the rest of the window shows the chosen view. Terminals a
//! view shows -- an agent's, a session's, a job's transcript -- are docked
//! read-only beside or under its list.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iced::widget::{Space, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Color, Element, Length, Task};
use zeughaus_link::{
    ActivityKind, ActivityState, AgentRole, AgentState, CiRequest, SessionKind, SessionState,
    SessionView,
};
use zeughaus_mux::TerminalId;
use zeughaus_theme::Theme;

use super::{CiClient, Tone, glyph, now, unavailable};
use crate::app::App;
use crate::app::agents::{attention_title, role_glyph, role_word, state_word};
use crate::message::{CiMsg, CiShortcut, Message};
use crate::notices::{self, Severity};
use crate::style::{self, Tokens};
use crate::workspace::RunnerKey;

/// The width of the page's sidebar.
const SIDEBAR_WIDTH: f32 = 240.0;
/// The width of the Overview's and the Sessions' lists.
const LIST_WIDTH: f32 = 420.0;
/// The width of a runner's activity beside the Runners table.
const ACTIVITY_WIDTH: f32 = 380.0;
/// The column a row's glyph stands in, so the words after it align.
const GLYPH_CELL: f32 = 18.0;
/// The width of a job's transcript beside its channel: the list keeps room
/// for a pipeline's job graph.
const TRANSCRIPT_WIDTH: f32 = 480.0;
/// Text of a row, and of a caption.
const ROW_TEXT: f32 = 13.0;
const CAPTION_TEXT: f32 = 12.0;
/// A session that failed longer ago than this needs nobody any more.
const FAILED_SESSION_RECENT: Duration = Duration::from_secs(24 * 3600);

/// What the page shows. A CI is named by its link's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PageView {
    /// What needs the user and what is working.
    Overview,
    /// A channel's pipelines, by the CI's view id.
    Channel(RunnerKey, u64),
    /// A repository's plans and proposals, by the CI's view id.
    Plans(RunnerKey, u64),
    /// The CI's agent sessions.
    Sessions(RunnerKey),
    /// The CI's runners and what each runs.
    Runners(RunnerKey),
}

/// What a row of the Overview opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttentionTarget {
    /// The tab of a runner's terminal.
    Terminal(RunnerKey, TerminalId),
    /// A view of the page.
    Page(PageView),
}

/// The page's state.
pub(in crate::app) struct Page {
    pub(in crate::app) open: bool,
    pub(in crate::app) view: PageView,
    /// The row under the keyboard of the Overview, Sessions or Runners.
    pub(in crate::app) cursor: usize,
}

impl Default for Page {
    fn default() -> Page {
        Page {
            open: false,
            view: PageView::Overview,
            cursor: 0,
        }
    }
}

/// One row of the Overview.
pub(in crate::app) struct AttentionItem {
    pub(in crate::app) severity: Severity,
    pub(in crate::app) glyph: char,
    pub(in crate::app) title: String,
    pub(in crate::app) detail: String,
    /// Unix seconds.
    pub(in crate::app) since: Option<u64>,
    pub(in crate::app) target: AttentionTarget,
}

/// The role glyph of a session kind.
fn session_glyph(kind: SessionKind) -> char {
    role_glyph(match kind {
        SessionKind::Plan => AgentRole::Plan,
        SessionKind::Master => AgentRole::Master,
    })
}

/// `plan zeughaus #3`.
fn session_name(session: &SessionView) -> String {
    let doc = session.doc.map(|n| format!(" #{n}")).unwrap_or_default();
    format!("{} {}{doc}", session.kind.name(), session.repo)
}

/// The live sessions, then the ended ones, newest first.
fn sessions_in_order(client: &CiClient) -> Vec<&SessionView> {
    let mut sessions: Vec<&SessionView> = client
        .overview
        .iter()
        .flat_map(|overview| &overview.sessions)
        .collect();
    sessions.sort_by_key(|s| (!s.state.is_live(), std::cmp::Reverse(s.started)));
    sessions
}

/// Seconds as `notices::age` reads them.
fn age_of(since: u64) -> String {
    notices::age(UNIX_EPOCH + Duration::from_secs(since), SystemTime::now())
}

/// A row's glyph in `color`, centred in its column.
fn glyph_cell<'a>(glyph: char, color: Color) -> Element<'a, Message, Theme> {
    container(super::glyph_text(glyph, 14.0).color(color))
        .width(GLYPH_CELL)
        .center_x(GLYPH_CELL)
        .into()
}

/// When the newest pipeline a channel view has loaded ended, or started
/// while it runs.
fn channel_since(client: &CiClient, view: u64) -> Option<u64> {
    let newest = &client.views.get(&view)?.pipelines.first()?.pipeline;
    Some(newest.finished.unwrap_or(newest.created))
}

impl App {
    /// What needs the user: agents that wait, plans to approve, failed
    /// channels and sessions. Errors first, then the newest.
    pub(in crate::app) fn needs_you(&self) -> Vec<AttentionItem> {
        let mut items = Vec::new();
        for (key, entry) in self.runtime.agents() {
            let Some(agent) = entry.agent.as_ref().filter(|a| a.needs_attention()) else {
                continue;
            };
            items.push(AttentionItem {
                severity: if agent.state == AgentState::Failed {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                glyph: role_glyph(agent.role),
                title: attention_title(agent),
                detail: agent.detail.clone(),
                since: Some(agent.at / 1000),
                target: AttentionTarget::Terminal(key.clone(), TerminalId(entry.terminal)),
            });
        }
        let now = now();
        for (ci, client) in &self.ci {
            let Some(overview) = &client.overview else {
                continue;
            };
            for summary in overview.repos.iter().filter(|r| r.awaiting > 0) {
                let Some(repo) = client.repos.get(&summary.repo) else {
                    continue;
                };
                let n = summary.awaiting;
                items.push(AttentionItem {
                    severity: Severity::Info,
                    glyph: glyph::PASS,
                    title: format!(
                        "{}: {n} plan{} await{} approval",
                        summary.repo,
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "s" } else { "" }
                    ),
                    detail: String::new(),
                    since: client.awaiting_since(repo.plans),
                    target: AttentionTarget::Page(PageView::Plans(ci.clone(), repo.plans)),
                });
            }
            for channel in &overview.channels {
                if !matches!(channel.status.as_str(), "failed" | "error") {
                    continue;
                }
                let Some(view) = client
                    .repos
                    .get(&channel.repo)
                    .and_then(|repo| repo.channels.get(&channel.channel))
                else {
                    continue;
                };
                items.push(AttentionItem {
                    severity: Severity::Error,
                    glyph: glyph::ERROR,
                    title: format!(
                        "{}/{} #{} {}",
                        channel.repo, channel.channel, channel.latest, channel.status
                    ),
                    detail: channel.load.clone().unwrap_or_default(),
                    since: channel_since(client, *view),
                    target: AttentionTarget::Page(PageView::Channel(ci.clone(), *view)),
                });
            }
            for session in &overview.sessions {
                let recent = session.ended.is_some_and(|ended| {
                    now.saturating_sub(ended) < FAILED_SESSION_RECENT.as_secs()
                });
                if session.state != SessionState::Failed || !recent {
                    continue;
                }
                items.push(AttentionItem {
                    severity: Severity::Error,
                    glyph: session_glyph(session.kind),
                    title: format!("{} failed", session_name(session)),
                    detail: session.message.clone(),
                    since: session.ended,
                    target: AttentionTarget::Page(PageView::Sessions(ci.clone())),
                });
            }
        }
        items.sort_by_key(|item| {
            (
                item.severity != Severity::Error,
                std::cmp::Reverse(item.since),
            )
        });
        items
    }

    /// What is working: agents in a turn, running channels, and live
    /// sessions whose agent reports nothing.
    pub(in crate::app) fn working(&self) -> Vec<AttentionItem> {
        let mut items = Vec::new();
        for (key, entry) in self.runtime.agents() {
            let Some(agent) = entry
                .agent
                .as_ref()
                .filter(|a| a.state == AgentState::Working)
            else {
                continue;
            };
            let doc = agent.doc.map(|n| format!(" #{n}")).unwrap_or_default();
            items.push(AttentionItem {
                severity: Severity::Info,
                glyph: role_glyph(agent.role),
                title: format!("{} · {}{doc}", agent.repo, role_word(agent.role)),
                detail: agent.detail.clone(),
                since: Some(agent.at / 1000),
                target: AttentionTarget::Terminal(key.clone(), TerminalId(entry.terminal)),
            });
        }
        for (ci, client) in &self.ci {
            let Some(overview) = &client.overview else {
                continue;
            };
            for channel in overview.channels.iter().filter(|c| c.status == "running") {
                let Some(view) = client
                    .repos
                    .get(&channel.repo)
                    .and_then(|repo| repo.channels.get(&channel.channel))
                else {
                    continue;
                };
                items.push(AttentionItem {
                    severity: Severity::Info,
                    glyph: glyph::RUNNING,
                    title: format!("{}/{} #{}", channel.repo, channel.channel, channel.latest),
                    detail: channel.load.clone().unwrap_or_default(),
                    since: channel_since(client, *view),
                    target: AttentionTarget::Page(PageView::Channel(ci.clone(), *view)),
                });
            }
            for session in overview.sessions.iter().filter(|s| s.state.is_live()) {
                let runner = client.runner_key(&session.runner);
                let reported = runner.as_ref().is_some_and(|runner| {
                    self.runtime.activity.get(runner).is_some_and(|entries| {
                        entries
                            .iter()
                            .any(|e| Some(e.terminal) == session.terminal && e.agent.is_some())
                    })
                });
                if reported {
                    continue;
                }
                let target = match (runner, session.terminal) {
                    (Some(runner), Some(terminal)) => {
                        AttentionTarget::Terminal(runner, TerminalId(terminal))
                    }
                    _ => AttentionTarget::Page(PageView::Sessions(ci.clone())),
                };
                items.push(AttentionItem {
                    severity: Severity::Info,
                    glyph: session_glyph(session.kind),
                    title: session_name(session),
                    detail: format!("{} on {}", session.state.name(), session.runner),
                    since: Some(session.started),
                    target,
                });
            }
        }
        items
    }

    /// The Overview's rows in the order they are listed.
    fn overview_items(&self) -> Vec<AttentionItem> {
        let mut items = self.needs_you();
        items.extend(self.working());
        items
    }

    /// How many rows the view in front lists, for its cursor.
    fn page_rows(&self) -> usize {
        match &self.page.view {
            PageView::Overview => self.overview_items().len(),
            PageView::Sessions(ci) => self
                .ci
                .get(ci)
                .map_or(0, |client| sessions_in_order(client).len()),
            PageView::Runners(ci) => self
                .ci
                .get(ci)
                .and_then(|client| client.overview.as_ref())
                .map_or(0, |overview| overview.machines.len()),
            PageView::Channel(..) | PageView::Plans(..) => 0,
        }
    }

    /// The terminals the open page docks that run on `key`: what its mux
    /// has to stream besides the terminals of its workspace.
    pub(in crate::app) fn page_terminals(&self, key: &RunnerKey) -> Vec<TerminalId> {
        if !self.page.open {
            return Vec::new();
        }
        let docked = match &self.page.view {
            PageView::Overview => match self
                .overview_items()
                .into_iter()
                .nth(self.page.cursor)
                .map(|item| item.target)
            {
                Some(AttentionTarget::Terminal(runner, terminal)) => Some((runner, terminal)),
                _ => None,
            },
            PageView::Sessions(ci) => self.selected_session(ci),
            PageView::Plans(ci, view) => self.docked_master(ci, *view),
            PageView::Channel(..) | PageView::Runners(_) => None,
        };
        docked
            .filter(|(runner, _)| runner == key)
            .map(|(_, terminal)| terminal)
            .into_iter()
            .collect()
    }

    /// The runner and terminal of the session under the cursor of a CI's
    /// Sessions.
    fn selected_session(&self, ci: &RunnerKey) -> Option<(RunnerKey, TerminalId)> {
        let client = self.ci.get(ci)?;
        let session = *sessions_in_order(client).get(self.page.cursor)?;
        Some((
            client.runner_key(&session.runner)?,
            TerminalId(session.terminal?),
        ))
    }

    /// Streams what the page docks now: every runner's terminals again.
    fn redock(&mut self) -> Task<Message> {
        let keys: Vec<RunnerKey> = self.mux.keys().cloned().collect();
        Task::batch(
            keys.iter()
                .map(|key| self.reconcile_terminals(key))
                .collect::<Vec<_>>(),
        )
    }

    /// What the page's messages do.
    pub(in crate::app) fn apply_page(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TogglePage => {
                self.page.open = !self.page.open;
            }
            Message::PageShow(view) => {
                self.page.open = true;
                if self.page.view != view {
                    self.page.view = view;
                    self.page.cursor = 0;
                }
            }
            Message::PageCursor(at) => {
                self.page.cursor = at;
            }
            Message::PageStep(step) => {
                if let PageView::Plans(ci, view) = self.page.view.clone() {
                    return Task::batch([self.plans_step(&ci, view, step), self.redock()]);
                }
                let rows = self.page_rows();
                if rows > 0 {
                    let at = (self.page.cursor as i64 + i64::from(step)).clamp(0, rows as i64 - 1);
                    self.page.cursor = at as usize;
                }
            }
            Message::PageEnter => match self.page.view.clone() {
                PageView::Overview => {
                    if let Some(item) = self.overview_items().into_iter().nth(self.page.cursor) {
                        return self.apply_page(Message::PageOpen(item.target));
                    }
                }
                PageView::Sessions(ci) => {
                    if let Some((runner, terminal)) = self.selected_session(&ci) {
                        return self.apply_page(Message::PageOpen(AttentionTarget::Terminal(
                            runner, terminal,
                        )));
                    }
                }
                PageView::Channel(..) => return self.toggle_front_ci_cursor(),
                PageView::Plans(..) | PageView::Runners(_) => {}
            },
            Message::PageOpen(AttentionTarget::Terminal(runner, terminal)) => {
                self.page.open = false;
                let revealed = self.apply(Message::RevealTerminal(runner, terminal));
                return Task::batch([revealed, self.redock()]);
            }
            Message::PageOpen(AttentionTarget::Page(view)) => {
                return self.apply_page(Message::PageShow(view));
            }
            Message::PageTerminal(key, terminal, action) => {
                return self.apply_page_terminal_action(key, terminal, action);
            }
            _ => return Task::none(),
        }
        self.redock()
    }

    /// The page's letters: j and k move its cursor, and in Plans `a`, `r`
    /// and `p` approve, reject and plan the selected document. `None` for a
    /// letter the page leaves to the channel in front.
    pub(in crate::app) fn page_shortcut(&mut self, shortcut: CiShortcut) -> Option<Task<Message>> {
        if !self.page.open {
            return None;
        }
        let step = match shortcut {
            CiShortcut::Next => Some(1),
            CiShortcut::Previous => Some(-1),
            _ => None,
        };
        match (&self.page.view, step) {
            (PageView::Channel(..), Some(step)) => Some(self.step_front_ci_cursor(step)),
            (_, Some(step)) => Some(self.apply_page(Message::PageStep(step as i32))),
            (PageView::Plans(ci, view), None) => {
                let (ci, view) = (ci.clone(), *view);
                Some(self.plans_letter(&ci, view, shortcut))
            }
            (PageView::Channel(..), None) => None,
            (_, None) => Some(Task::none()),
        }
    }

    /// The page: its sidebar beside the view it shows.
    pub(in crate::app) fn page_view(&self) -> Element<'_, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let sidebar = container(scrollable(self.page_sidebar(&t)).height(Length::Fill))
            .width(SIDEBAR_WIDTH)
            .height(Length::Fill)
            .padding(12)
            .style(style::solid(t.sidebar));
        let main: Element<'_, Message, Theme> = match &self.page.view {
            PageView::Overview => self.overview_view(&t),
            PageView::Channel(ci, view) => self.page_channel(ci, *view),
            PageView::Plans(ci, view) => match self.ci.get_key_value(ci) {
                Some((ci, client)) => match client.plans.get(view) {
                    Some(state) => self.plans_view(ci, client, *view, state),
                    None => unavailable("Plans", "This repository is gone."),
                },
                None => unavailable("Plans", "This CI is not connected."),
            },
            PageView::Sessions(ci) => self.sessions_view(&t, ci),
            PageView::Runners(ci) => self.page_runners(&t, ci),
        };
        let main = container(main)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style::solid(t.surface));
        row![sidebar, rule::vertical(1), main]
            .height(Length::Fill)
            .into()
    }

    /// An entry of the sidebar: its words and, when it has one, a count.
    fn sidebar_row<'a>(
        &'a self,
        t: &Tokens,
        dot: Option<Color>,
        words: String,
        count: Option<(u32, bool)>,
        view: PageView,
    ) -> Element<'a, Message, Theme> {
        let active = self.page.view == view;
        let mut line = row![].spacing(6).align_y(Alignment::Center);
        if let Some(color) = dot {
            line = line.push(
                container(Space::new())
                    .width(8)
                    .height(8)
                    .style(move |_: &Theme| container::Style {
                        background: Some(color.into()),
                        border: iced::border::rounded(4),
                        ..container::Style::default()
                    }),
            );
        }
        line = line.push(text(words).size(ROW_TEXT).width(Length::Fill));
        if let Some((n, accent)) = count.filter(|(n, _)| *n > 0) {
            line = line.push(
                container(text(n.to_string()).size(11))
                    .padding([0, 6])
                    .style(style::badge(t, accent)),
            );
        }
        container(
            button(line)
                .width(Length::Fill)
                .padding([4, 8])
                .style(style::row_entry_button(t, active))
                .on_press(Message::PageShow(view)),
        )
        .style(style::row_highlight(t, active))
        .into()
    }

    /// A caption over a group of sidebar entries.
    fn caption<'a>(&self, words: String) -> Element<'a, Message, Theme> {
        container(
            text(words.to_uppercase())
                .size(CAPTION_TEXT)
                .color(self.theme.muted()),
        )
        .padding([0, 8])
        .into()
    }

    fn page_sidebar(&self, t: &Tokens) -> Element<'_, Message, Theme> {
        let needs = self.needs_you().len() as u32;
        let working = self.working().len() as u32;
        let mut side = column![
            self.sidebar_row(
                t,
                None,
                "Needs you".to_owned(),
                Some((needs, true)),
                PageView::Overview
            ),
            self.overview_working_row(t, working),
        ]
        .spacing(6);
        for (ci, client) in &self.ci {
            let mut caption = row![self.caption(format!("CI {}", client.label))]
                .spacing(4)
                .align_y(Alignment::Center);
            if let Some(stale) = self.stale(client) {
                caption = caption.push(stale);
            }
            side = side.push(Space::new().height(8)).push(caption);
            for (name, repo) in &client.repos {
                side = side
                    .push(Space::new().height(2))
                    .push(self.caption(name.clone()));
                for view in repo.channels.values() {
                    let Some(state) = client.views.get(view) else {
                        continue;
                    };
                    let dot = client
                        .accent(state, &self.theme)
                        .unwrap_or(self.theme.muted());
                    side = side.push(self.sidebar_row(
                        t,
                        Some(dot),
                        state.channel.clone(),
                        None,
                        PageView::Channel(ci.clone(), *view),
                    ));
                }
                let awaiting = client
                    .overview
                    .as_ref()
                    .and_then(|o| o.repos.iter().find(|r| &r.repo == name))
                    .map_or(0, |r| r.awaiting);
                side = side.push(self.sidebar_row(
                    t,
                    None,
                    "Plans".to_owned(),
                    Some((awaiting, true)),
                    PageView::Plans(ci.clone(), repo.plans),
                ));
            }
            let live = client.overview.as_ref().map_or(0, |o| {
                o.sessions.iter().filter(|s| s.state.is_live()).count()
            }) as u32;
            side = side
                .push(Space::new().height(2))
                .push(self.sidebar_row(
                    t,
                    None,
                    "Sessions".to_owned(),
                    Some((live, false)),
                    PageView::Sessions(ci.clone()),
                ))
                .push(self.sidebar_row(
                    t,
                    None,
                    "Runners".to_owned(),
                    None,
                    PageView::Runners(ci.clone()),
                ));
        }
        side.into()
    }

    /// `Working` in the sidebar: the Overview too, lit with it.
    fn overview_working_row(&self, t: &Tokens, working: u32) -> Element<'_, Message, Theme> {
        let active = self.page.view == PageView::Overview;
        let mut line =
            row![text("Working").size(ROW_TEXT).width(Length::Fill)].align_y(Alignment::Center);
        if working > 0 {
            line = line.push(
                container(text(working.to_string()).size(11))
                    .padding([0, 6])
                    .style(style::badge(t, false)),
            );
        }
        container(
            button(line)
                .width(Length::Fill)
                .padding([4, 8])
                .style(style::row_entry_button(t, active))
                .on_press(Message::PageShow(PageView::Overview)),
        )
        .into()
    }

    /// The Overview: `Needs you` and `Working`, and beside them the
    /// terminal of the row under the cursor.
    fn overview_view(&self, t: &Tokens) -> Element<'_, Message, Theme> {
        let needs = self.needs_you();
        let working = self.working();
        let mut list = column![].spacing(6).padding(12);
        let mut at = 0;
        let mut selected = None;
        for (caption, items, empty) in [
            ("Needs you", needs, "Nothing needs you."),
            ("Working", working, "Nothing is working."),
        ] {
            list = list.push(self.caption(caption.to_owned()));
            if items.is_empty() {
                list = list.push(
                    container(text(empty).size(ROW_TEXT).color(self.theme.muted())).padding([4, 8]),
                );
            }
            for item in items {
                let active = at == self.page.cursor;
                if active {
                    selected = Some(item.target.clone());
                }
                list = list.push(self.attention_row(t, item, active));
                at += 1;
            }
            list = list.push(Space::new().height(8));
        }
        let detail: Element<'_, Message, Theme> = match selected {
            Some(AttentionTarget::Terminal(runner, terminal)) => {
                self.docked(t, &runner, terminal, "Bring its tab to the front")
            }
            Some(AttentionTarget::Page(_)) => container(
                text("Enter or a click opens it.")
                    .size(ROW_TEXT)
                    .color(self.theme.muted()),
            )
            .center(Length::Fill)
            .into(),
            None => Space::new().into(),
        };
        row![
            scrollable(list).width(LIST_WIDTH).height(Length::Fill),
            rule::vertical(1),
            container(detail).width(Length::Fill).height(Length::Fill),
        ]
        .into()
    }

    /// One row of the Overview: the glyph in its severity's colour, the
    /// title over the detail, and the age.
    fn attention_row<'a>(
        &'a self,
        t: &Tokens,
        item: AttentionItem,
        active: bool,
    ) -> Element<'a, Message, Theme> {
        let color = match item.severity {
            Severity::Info => self.theme.accent(),
            severity => style::severity_color(&self.theme, severity),
        };
        let mut words = column![text(item.title).size(ROW_TEXT)].spacing(2);
        if !item.detail.is_empty() {
            words = words.push(
                text(item.detail)
                    .size(CAPTION_TEXT)
                    .color(self.theme.muted()),
            );
        }
        let mut line = row![glyph_cell(item.glyph, color), words.width(Length::Fill),]
            .spacing(8)
            .align_y(Alignment::Start);
        if let Some(since) = item.since {
            line = line.push(
                text(age_of(since))
                    .size(CAPTION_TEXT)
                    .color(self.theme.muted()),
            );
        }
        container(
            button(line)
                .width(Length::Fill)
                .padding([6, 8])
                .style(style::row_entry_button(t, active))
                .on_press(Message::PageOpen(item.target)),
        )
        .style(style::row_highlight(t, active))
        .into()
    }

    /// A runner's terminal docked read-only under a bar with its title and
    /// a button that brings its tab to the front.
    fn docked<'a>(
        &'a self,
        t: &Tokens,
        runner: &RunnerKey,
        terminal: TerminalId,
        what: &'a str,
    ) -> Element<'a, Message, Theme> {
        let title = self
            .terminal_view(runner, terminal)
            .and_then(|(view, _)| {
                let guard = view.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_ref().map(|v| v.title.clone())
            })
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| format!("terminal {}", terminal.0));
        let open = super::tip(
            button(text("Open tab").size(CAPTION_TEXT))
                .padding([3, 12])
                .style(style::pill_button(t))
                .on_press(Message::RevealTerminal(runner.clone(), terminal)),
            what.to_owned(),
        );
        let bar = row![text(title).size(ROW_TEXT).width(Length::Fill), open]
            .spacing(8)
            .align_y(Alignment::Center)
            .padding([6, 12]);
        column![
            bar,
            rule::horizontal(1),
            self.page_terminal(runner, terminal)
        ]
        .height(Length::Fill)
        .into()
    }

    /// A channel: its list, and its job's transcript beside it once one is
    /// open.
    fn page_channel<'a>(&'a self, ci: &RunnerKey, view: u64) -> Element<'a, Message, Theme> {
        let Some((ci, client)) = self.ci.get_key_value(ci) else {
            return unavailable("CI", "This CI is not connected.");
        };
        let Some(state) = client.views.get(&view) else {
            return unavailable("CI", "This channel is gone.");
        };
        let list = self.channel_view(ci, client, view, state);
        match state.transcript.as_ref().and_then(|t| t.terminal) {
            Some(terminal) => row![
                container(list).width(Length::Fill),
                rule::vertical(1),
                container(self.page_terminal(ci, TerminalId(terminal)))
                    .width(TRANSCRIPT_WIDTH)
                    .height(Length::Fill),
            ]
            .into(),
            None => list,
        }
    }

    /// A CI's agent sessions: live ones first, then the ended ones, newest
    /// first, and the terminal of the one under the cursor.
    fn sessions_view<'a>(&'a self, t: &Tokens, ci: &RunnerKey) -> Element<'a, Message, Theme> {
        let Some((ci, client)) = self.ci.get_key_value(ci) else {
            return unavailable("Sessions", "This CI is not connected.");
        };
        let sessions = sessions_in_order(client);
        if sessions.is_empty() {
            return unavailable(
                "Sessions",
                "No agent sessions yet: a plan session or an approved plan starts one.",
            );
        }
        let muted = self.theme.muted();
        let mut list = column![self.caption("Sessions".to_owned())]
            .spacing(6)
            .padding(12);
        for (at, session) in sessions.iter().enumerate() {
            let active = at == self.page.cursor;
            let tone = match session.state {
                SessionState::Failed => Tone::Bad,
                SessionState::Running | SessionState::Starting => Tone::Active,
                SessionState::Ended => Tone::Idle,
            };
            let line = row![
                glyph_cell(session_glyph(session.kind), tone.color(&self.theme)),
                column![
                    text(session_name(session)).size(ROW_TEXT),
                    text(format!("{} on {}", session.state.name(), session.runner))
                        .size(CAPTION_TEXT)
                        .color(muted),
                ]
                .spacing(2)
                .width(Length::Fill),
                text(age_of(session.ended.unwrap_or(session.started)))
                    .size(CAPTION_TEXT)
                    .color(muted),
            ]
            .spacing(8)
            .align_y(Alignment::Start);
            list = list.push(
                container(
                    button(line)
                        .width(Length::Fill)
                        .padding([6, 8])
                        .style(style::row_entry_button(t, active))
                        .on_press(Message::PageCursor(at)),
                )
                .style(style::row_highlight(t, active)),
            );
        }
        let detail: Element<'a, Message, Theme> = match sessions.get(self.page.cursor) {
            Some(session) => self.session_detail(t, ci, client, session),
            None => Space::new().into(),
        };
        row![
            scrollable(list).width(LIST_WIDTH).height(Length::Fill),
            rule::vertical(1),
            container(detail).width(Length::Fill).height(Length::Fill),
        ]
        .into()
    }

    /// A session: its terminal read-only with `Open tab`, and `Stop` while
    /// it lives.
    fn session_detail<'a>(
        &'a self,
        t: &Tokens,
        ci: &'a RunnerKey,
        client: &'a CiClient,
        session: &SessionView,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut bar = row![
            text(session_name(session))
                .size(ROW_TEXT)
                .width(Length::Fill)
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([6, 12]);
        let terminal = client
            .runner_key(&session.runner)
            .zip(session.terminal.map(TerminalId));
        if let Some((runner, terminal)) = &terminal {
            bar = bar.push(
                button(text("Open tab").size(CAPTION_TEXT))
                    .padding([3, 12])
                    .style(style::pill_button(t))
                    .on_press(Message::RevealTerminal(runner.clone(), *terminal)),
            );
        }
        if session.state.is_live()
            && let Some(view) = client.repos.get(&session.repo).map(|r| r.plans)
        {
            let request = CiRequest::CloseSession { id: session.id };
            bar = bar.push(
                button(text("Stop").size(CAPTION_TEXT))
                    .padding([3, 12])
                    .style(style::danger_button(t))
                    .on_press_with(move || {
                        Message::Ci(
                            ci.clone(),
                            CiMsg::DocAct {
                                view,
                                request: request.clone(),
                            },
                        )
                    }),
            );
        }
        let body: Element<'a, Message, Theme> = match (session.state.is_live(), terminal) {
            (true, Some((runner, terminal))) => self.page_terminal(&runner, terminal),
            _ => {
                let said = if session.message.is_empty() {
                    session.state.name().to_owned()
                } else {
                    session.message.clone()
                };
                container(text(said).size(ROW_TEXT).color(muted))
                    .padding(12)
                    .into()
            }
        };
        column![bar, rule::horizontal(1), body]
            .height(Length::Fill)
            .into()
    }

    /// A CI's runners, and what the one under the cursor runs.
    fn page_runners<'a>(&'a self, t: &Tokens, ci: &RunnerKey) -> Element<'a, Message, Theme> {
        let Some((ci, client)) = self.ci.get_key_value(ci) else {
            return unavailable("Runners", "This CI is not connected.");
        };
        let list = self.runners_view(ci, client, self.page.cursor);
        let selected = client
            .overview
            .as_ref()
            .and_then(|overview| overview.machines.get(self.page.cursor));
        let detail: Element<'a, Message, Theme> = match selected {
            Some(machine) => self.runner_activity(t, client, &machine.name),
            None => Space::new().into(),
        };
        row![
            container(list).width(Length::Fill),
            rule::vertical(1),
            container(detail).width(ACTIVITY_WIDTH).height(Length::Fill),
        ]
        .into()
    }

    /// What runner `name` runs and ran: its terminals as its `/activity`
    /// says, maintenance runs included, with the closed ones to reopen.
    fn runner_activity<'a>(
        &'a self,
        t: &Tokens,
        client: &CiClient,
        name: &str,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let Some(key) = client.runner_key(name) else {
            return container(
                text("This editor does not reach that runner.")
                    .size(ROW_TEXT)
                    .color(muted),
            )
            .padding(12)
            .into();
        };
        let entries = self.runtime.activity.get(&key);
        let mut list = column![self.caption(format!("{name}: activity"))]
            .spacing(6)
            .padding(12);
        if entries.is_none_or(Vec::is_empty) {
            list = list.push(
                container(text("Nothing reported yet.").size(ROW_TEXT).color(muted))
                    .padding([4, 8]),
            );
        }
        for entry in entries.into_iter().flatten() {
            let kind = match entry.kind {
                ActivityKind::Shell => '\u{ea85}',
                ActivityKind::Session => role_glyph(
                    entry
                        .agent
                        .as_ref()
                        .map_or(AgentRole::Master, |agent| agent.role),
                ),
                ActivityKind::Job | ActivityKind::Trigger => glyph::RUNNING,
                ActivityKind::CiJob => glyph::BRANCH,
                ActivityKind::Maintenance => '\u{eb6d}',
            };
            let mut state = match entry.state {
                ActivityState::Running => "running".to_owned(),
                ActivityState::Exited => "exited".to_owned(),
                ActivityState::Closed => "closed".to_owned(),
            };
            if let Some(agent) = &entry.agent {
                state.push_str(&format!(", agent {}", state_word(agent.state)));
            }
            let when = age_of(entry.ended.unwrap_or(entry.started));
            let mut line = row![
                glyph_cell(kind, muted),
                column![
                    text(entry.label.as_str()).size(ROW_TEXT),
                    text(state).size(CAPTION_TEXT).color(muted),
                ]
                .spacing(2)
                .width(Length::Fill),
                text(when).size(CAPTION_TEXT).color(muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if let (ActivityState::Closed, Some(run)) = (entry.state, entry.run) {
                line = line.push(
                    button(text("Reopen").size(CAPTION_TEXT))
                        .padding([3, 10])
                        .style(style::row_action_button(t))
                        .on_press(Message::ReplayRun(key.clone(), run)),
                );
            }
            list = list.push(container(line).padding([4, 8]));
        }
        scrollable(list).height(Length::Fill).into()
    }

    /// The titlebar's chip: a dot in the colour of the worst that needs the
    /// user (else the accent while something works), and the counts. `None`
    /// without a CI and without an agent.
    pub(in crate::app) fn page_chip(&self) -> Option<Element<'_, Message, Theme>> {
        if self.ci.is_empty() && self.runtime.agents().next().is_none() {
            return None;
        }
        let t = Tokens::of(&self.theme);
        let needs = self.needs_you();
        let working = self.working().len();
        let worst = needs.iter().map(|item| item.severity).min();
        let color = match worst {
            Some(Severity::Info) => self.theme.accent(),
            Some(severity) => style::severity_color(&self.theme, severity),
            None if working > 0 => self.theme.accent(),
            None => self.theme.muted(),
        };
        let mut words = Vec::new();
        if !needs.is_empty() {
            words.push(format!("{} need you", needs.len()));
        }
        if working > 0 {
            words.push(format!("{working} working"));
        }
        let words = if words.is_empty() {
            "CI".to_owned()
        } else {
            words.join(" · ")
        };
        let dot = container(Space::new())
            .width(8)
            .height(8)
            .style(move |_: &Theme| container::Style {
                background: Some(color.into()),
                border: iced::border::rounded(4),
                ..container::Style::default()
            });
        let content = row![dot, text(words).size(CAPTION_TEXT)]
            .spacing(6)
            .align_y(Alignment::Center);
        let chip = button(content)
            .padding([2, 10])
            .on_press(Message::TogglePage);
        let chip = if self.page.open {
            chip.style(style::pill_button(&t))
        } else {
            chip.style(style::secondary_button(&t))
        };
        Some(chip.into())
    }
}
