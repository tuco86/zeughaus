//! The CI views: the Overview the titlebar chip opens, a project's Board and
//! a channel's pipelines, the last two as tabs of the project's group in the
//! bar. Terminals a view shows -- an agent's, a master's, a job's transcript
//! -- are docked read-only beside or under it.
//!
//! The Board is a project's lifecycle on one screen, read left to right:
//! proposals become plans, plans queue, run and finish, with the channels'
//! health above. A card the CI moves to another column arrives with an
//! accent ring that fades over [`MOVED_FOR`].

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iced::widget::text::Wrapping;
use iced::widget::{
    Space, button, column, container, mouse_area, row, rule, scrollable, text, text_input,
};
use iced::{Alignment, Border, Color, Element, Length, Task};
use zeughaus_link::{
    ActivityKind, ActivityState, AgentRole, AgentState, CiRequest, DocGroup, DocKind, DocView,
    OnboardSpec, RepoDocsView, SessionKind, SessionState, SessionView, doc_groups,
};
use zeughaus_mux::TerminalId;
use zeughaus_theme::Theme;

use super::{CiClient, Tone, glyph, now, unavailable};
use crate::app::App;
use crate::app::agents::{agent_color, attention_title, role_glyph, role_word, state_word};
use crate::message::{CiShortcut, Message};
use crate::notices::{self, Severity};
use crate::style::{self, Tokens};
use crate::workspace::RunnerKey;

/// The width of the Overview's list.
const LIST_WIDTH: f32 = 420.0;
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
/// The Board's rail: a row per channel and the pipelines each shows.
const RAIL_ROW: f32 = 28.0;
const RAIL_PIPELINES: usize = 12;
const PIPELINE_DOT: f32 = 8.0;
/// The finished documents the DONE column shows.
const DONE_SHOWN: usize = 12;
/// How long a card the CI moved carries its ring.
const MOVED_FOR: Duration = Duration::from_secs(2);

/// The glyphs of a project's channels: dev, nightly, release, and any
/// other.
const GLYPH_DEV: char = '\u{eafc}';
const GLYPH_NIGHTLY: char = '\u{f0594}';
const GLYPH_RELEASE: char = '\u{ea66}';
const GLYPH_CHANNEL: char = '\u{ea71}';

/// The Overview's project search.
pub(in crate::app) const PROJECT_SEARCH: iced::widget::Id = iced::widget::Id::new("project-search");

/// What the page shows. A CI is named by its link's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PageView {
    /// What needs the user, what is working, the projects and the runners.
    Overview,
    /// A project's Board, by its origin.
    Board(String),
    /// A channel's pipelines, by the CI's view id.
    Channel(RunnerKey, u64),
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
    /// The row under the keyboard of the Overview.
    pub(in crate::app) cursor: usize,
    /// The runner of a CI whose activity the Overview shows: the CI and the
    /// machine's place in its overview.
    pub(in crate::app) machine: Option<(RunnerKey, usize)>,
    pub(in crate::app) board: Board,
}

impl Default for Page {
    fn default() -> Page {
        Page {
            open: false,
            view: PageView::Overview,
            cursor: 0,
            machine: None,
            board: Board::default(),
        }
    }
}

/// The Board's cursor and what it remembers of the cards.
#[derive(Default)]
pub(in crate::app) struct Board {
    /// The project the cursor is in.
    origin: String,
    column: usize,
    row: usize,
    /// Whether the selected document's detail is open beside the board.
    detail: bool,
    /// The column each document was last seen in, by repository and number.
    columns: HashMap<(String, u64), usize>,
    /// When the CI moved a document to another column.
    moved: HashMap<(String, u64), Instant>,
    /// The card the cursor is on: the cursor follows it to another column
    /// when the CI moves it.
    card: Option<Card>,
}

impl Board {
    /// Where the cursor is: on its card wherever that is now, else at its
    /// last place.
    fn at(&self, cards: &[Vec<Card>; 5]) -> (usize, usize) {
        if let Some(card) = self.card {
            for (column, list) in cards.iter().enumerate() {
                if let Some(row) = list.iter().position(|c| *c == card) {
                    return (column, row);
                }
            }
        }
        (self.column, self.row)
    }
}

/// One card of the Board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Card {
    Doc(u64),
    /// A live plan session, by its id.
    Session(u64),
}

/// The columns of the Board, left to right.
const COLUMNS: [&str; 5] = ["Proposals", "Plans", "Queue", "Running", "Done"];

/// The column of a document.
fn column_of(group: DocGroup) -> usize {
    match group {
        DocGroup::Proposals => 0,
        DocGroup::Awaiting => 1,
        DocGroup::Queue => 2,
        DocGroup::InProgress => 3,
        DocGroup::History => 4,
    }
}

/// What a card says.
#[derive(Default)]
struct CardFace<'a> {
    head: String,
    changed: Option<u64>,
    runs_on: &'a [String],
    /// The document's repository and number, for its ring.
    key: Option<(String, u64)>,
}

/// A repository's cards in their columns: its documents and its live plan
/// sessions.
fn board_columns(docs: &[DocView], sessions: &[&SessionView]) -> [Vec<Card>; 5] {
    let mut columns: [Vec<Card>; 5] = Default::default();
    for (group, docs) in doc_groups(docs) {
        let column = &mut columns[column_of(group)];
        let shown = if group == DocGroup::History {
            DONE_SHOWN
        } else {
            usize::MAX
        };
        column.extend(
            docs.into_iter()
                .take(shown)
                .map(|doc| Card::Doc(doc.number)),
        );
    }
    columns[3].extend(sessions.iter().map(|s| Card::Session(s.id)));
    columns
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

/// The glyph of a channel.
fn channel_glyph(channel: &str) -> char {
    match channel {
        "dev" => GLYPH_DEV,
        "nightly" => GLYPH_NIGHTLY,
        "release" => GLYPH_RELEASE,
        _ => GLYPH_CHANNEL,
    }
}

/// A channel badge of a project's group header.
pub(in crate::app) struct ChannelBadge {
    pub(in crate::app) channel: String,
    pub(in crate::app) glyph: char,
    pub(in crate::app) color: Color,
    pub(in crate::app) tooltip: String,
}

/// The origin of the repository a CI names `repo`, falling back to the
/// name: what a Board is opened by.
fn origin_of(client: &CiClient, repo: &str) -> String {
    client
        .repo_docs_named(repo)
        .map_or_else(|| repo.to_owned(), |known| known.origin.clone())
}

impl App {
    /// The CI that builds `origin`, and its repository there.
    fn ci_of(&self, origin: &str) -> Option<(&RunnerKey, &CiClient, &RepoDocsView)> {
        self.ci.iter().find_map(|(ci, client)| {
            let repo = client.repo_docs_by_origin(origin)?;
            Some((ci, client, repo))
        })
    }

    /// The channels of `origin`'s repository as its group's header shows
    /// them, in the colours of how their newest pipeline went.
    pub(in crate::app) fn channel_badges(&self, origin: &str) -> Vec<ChannelBadge> {
        let Some((_, client, repo)) = self.ci_of(origin) else {
            return Vec::new();
        };
        client
            .channels_of(&repo.repo)
            .into_iter()
            .map(|channel| {
                let tooltip = if channel.latest == 0 {
                    format!("{}: no pipeline yet", channel.channel)
                } else {
                    format!("{} #{} {}", channel.channel, channel.latest, channel.status)
                };
                ChannelBadge {
                    channel: channel.channel.clone(),
                    glyph: channel_glyph(&channel.channel),
                    color: Tone::of(&channel.status).color(&self.theme),
                    tooltip,
                }
            })
            .collect()
    }

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
                    target: AttentionTarget::Page(PageView::Board(summary.origin.clone())),
                });
            }
            for channel in &overview.channels {
                if !matches!(channel.status.as_str(), "failed" | "error") {
                    continue;
                }
                let Some(view) = client.channel_view(&channel.repo, &channel.channel) else {
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
                    since: channel_since(client, view),
                    target: AttentionTarget::Page(PageView::Channel(ci.clone(), view)),
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
                    target: session_target(client, session),
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
                let Some(view) = client.channel_view(&channel.repo, &channel.channel) else {
                    continue;
                };
                items.push(AttentionItem {
                    severity: Severity::Info,
                    glyph: glyph::RUNNING,
                    title: format!("{}/{} #{}", channel.repo, channel.channel, channel.latest),
                    detail: channel.load.clone().unwrap_or_default(),
                    since: channel_since(client, view),
                    target: AttentionTarget::Page(PageView::Channel(ci.clone(), view)),
                });
            }
            for session in overview.sessions.iter().filter(|s| s.state.is_live()) {
                if self.session_agent(client, session).is_some() {
                    continue;
                }
                items.push(AttentionItem {
                    severity: Severity::Info,
                    glyph: session_glyph(session.kind),
                    title: session_name(session),
                    detail: format!("{} on {}", session.state.name(), session.runner),
                    since: Some(session.started),
                    target: session_target(client, session),
                });
            }
        }
        items
    }

    /// What the agent in a session's terminal reports, once it reports.
    fn session_agent(
        &self,
        client: &CiClient,
        session: &SessionView,
    ) -> Option<zeughaus_link::AgentStatus> {
        let runner = client.runner_key(&session.runner)?;
        let terminal = session.terminal?;
        self.runtime
            .activity
            .get(&runner)?
            .iter()
            .find(|e| e.terminal == terminal)?
            .agent
            .clone()
    }

    /// The Overview's attention rows in the order they are listed.
    pub(in crate::app) fn overview_items(&self) -> Vec<AttentionItem> {
        let mut items = self.needs_you();
        items.extend(self.working());
        items
    }

    /// The projects the Overview lists under its search.
    fn searched_projects(&self) -> Vec<crate::app::projects::Listed> {
        let needle = self.projects.search.trim().to_lowercase();
        self.listed_projects()
            .into_iter()
            .filter(|p| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.origin.contains(&needle)
            })
            .collect()
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
            PageView::Board(origin) if self.page.board.detail => {
                self.ci_of(origin).and_then(|(ci, client, repo)| {
                    self.docked_master(ci, client.plans_view_of(&repo.repo)?)
                })
            }
            PageView::Board(_) | PageView::Channel(..) => None,
        };
        docked
            .filter(|(runner, _)| runner == key)
            .map(|(_, terminal)| terminal)
            .into_iter()
            .collect()
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
            // The titlebar chip and the palette: the Overview.
            Message::TogglePage => {
                if self.page.open && self.page.view == PageView::Overview {
                    self.page.open = false;
                } else {
                    return self.apply_page(Message::PageShow(PageView::Overview));
                }
            }
            Message::PageShow(view) => {
                self.page.open = true;
                let asked = match &view {
                    PageView::Overview => self.ask_projects(false),
                    PageView::Board(origin) => {
                        // A Board is a tab of its project's group.
                        self.keep_project(origin);
                        if self.page.board.origin != *origin {
                            self.page.board.origin.clone_from(origin);
                            self.page.board.column = 0;
                            self.page.board.row = 0;
                            self.page.board.detail = false;
                            self.page.board.card = None;
                        }
                        Task::none()
                    }
                    PageView::Channel(ci, channel) => {
                        self.adopt_channel_view(ci, *channel);
                        Task::none()
                    }
                };
                if self.page.view != view {
                    self.page.view = view;
                    self.page.cursor = 0;
                }
                return Task::batch([asked, self.redock()]);
            }
            Message::PageCursor(at) => {
                self.page.cursor = at;
                self.page.machine = None;
            }
            Message::PageMachine(ci, at) => {
                self.page.machine = Some((ci, at));
            }
            Message::PageStep(step) => match self.page.view.clone() {
                PageView::Board(origin) => return self.board_move(&origin, 0, step as isize),
                PageView::Overview => {
                    let rows = self.overview_items().len() + self.searched_projects().len();
                    if rows > 0 {
                        let at =
                            (self.page.cursor as i64 + i64::from(step)).clamp(0, rows as i64 - 1);
                        self.page.cursor = at as usize;
                        self.page.machine = None;
                    }
                }
                PageView::Channel(..) => return self.step_front_ci_cursor(step as isize),
            },
            Message::PageEnter => match self.page.view.clone() {
                PageView::Overview => {
                    let items = self.overview_items();
                    let at = self.page.cursor;
                    if let Some(item) = items.into_iter().nth(at) {
                        return self.apply_page(Message::PageOpen(item.target));
                    }
                    let projects = at - self.overview_items().len();
                    if let Some(project) = self.searched_projects().into_iter().nth(projects) {
                        return self.show_project(project.origin);
                    }
                }
                PageView::Board(origin) => return self.board_enter(&origin),
                PageView::Channel(..) => return self.toggle_front_ci_cursor(),
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
            Message::PagePipeline(ci, view, number) => {
                if let Some(state) = self.ci.get_mut(&ci).and_then(|c| c.views.get_mut(&view)) {
                    state.cursor = Some(number);
                }
                return self.apply_page(Message::PageShow(PageView::Channel(ci, view)));
            }
            Message::BoardSelect(column, row) => {
                let PageView::Board(origin) = self.page.view.clone() else {
                    return Task::none();
                };
                self.page.board.column = column;
                self.page.board.row = row;
                self.page.board.card = None;
                return self.board_enter(&origin);
            }
            _ => return Task::none(),
        }
        self.redock()
    }

    /// The page's letters: j and k move its cursor; on a Board h and l (and
    /// the arrows) change the column and `a`, `r` and `p` approve, reject
    /// and plan the selected document. `None` for a letter the page leaves
    /// to the channel in front.
    pub(in crate::app) fn page_shortcut(&mut self, shortcut: CiShortcut) -> Option<Task<Message>> {
        if !self.page.open {
            return None;
        }
        let rows = match shortcut {
            CiShortcut::Next => Some(1),
            CiShortcut::Previous => Some(-1),
            _ => None,
        };
        match self.page.view.clone() {
            PageView::Channel(..) => match (rows, shortcut) {
                (Some(step), _) => Some(self.step_front_ci_cursor(step)),
                (None, CiShortcut::Left | CiShortcut::Right) => Some(Task::none()),
                (None, _) => None,
            },
            PageView::Overview => Some(match rows {
                Some(step) => self.apply_page(Message::PageStep(step as i32)),
                None => Task::none(),
            }),
            PageView::Board(origin) => Some(match shortcut {
                CiShortcut::Next | CiShortcut::Previous => {
                    self.board_move(&origin, 0, rows.unwrap_or(0))
                }
                CiShortcut::Left => self.board_move(&origin, -1, 0),
                // `l` is the log letter of a channel and right on a Board.
                CiShortcut::Right | CiShortcut::OpenLog => self.board_move(&origin, 1, 0),
                CiShortcut::Approve | CiShortcut::Rerun | CiShortcut::Plan => {
                    match self.board_doc(&origin) {
                        Some((ci, view, number)) => {
                            let selected = self.select_doc(&ci, view, number);
                            Task::batch([selected, self.plans_letter(&ci, view, shortcut)])
                        }
                        None => Task::none(),
                    }
                }
                CiShortcut::Cancel => Task::none(),
            }),
        }
    }

    /// What Escape does on the page when nothing nearer took it: closes the
    /// Overview, a Board's detail, a channel's transcript.
    pub(in crate::app) fn page_escape(&mut self) -> Task<Message> {
        if !self.page.open {
            return Task::none();
        }
        match self.page.view.clone() {
            PageView::Overview => {
                self.page.open = false;
                self.redock()
            }
            PageView::Board(_) => {
                self.page.board.detail = false;
                self.redock()
            }
            PageView::Channel(..) => self.close_front_ci_log(),
        }
    }

    /// The cards of `origin`'s Board and where they come from.
    fn board_cards(&self, origin: &str) -> Option<(RunnerKey, u64, [Vec<Card>; 5])> {
        let (ci, client, repo) = self.ci_of(origin)?;
        let view = client.plans_view_of(&repo.repo)?;
        let state = client.plans.get(&view)?;
        let sessions: Vec<&SessionView> = client
            .sessions()
            .filter(|s| s.repo == repo.repo && s.kind == SessionKind::Plan && s.state.is_live())
            .collect();
        Some((ci.clone(), view, board_columns(state.docs(), &sessions)))
    }

    /// The document under the Board's cursor, with its CI and Plans view.
    fn board_doc(&self, origin: &str) -> Option<(RunnerKey, u64, u64)> {
        let (ci, view, columns) = self.board_cards(origin)?;
        let (column, row) = self.page.board.at(&columns);
        match columns[column].get(row)? {
            Card::Doc(number) => Some((ci, view, *number)),
            Card::Session(_) => None,
        }
    }

    /// Selects a document of a Plans view, which asks for its body.
    fn select_doc(&mut self, ci: &RunnerKey, view: u64, number: u64) -> Task<Message> {
        let Some(client) = self.ci.get_mut(ci) else {
            return Task::none();
        };
        if client.plans.get(&view).and_then(|s| s.selected()) == Some(number) {
            return Task::none();
        }
        client.select(ci, view, number)
    }

    /// Moves the Board's cursor by columns and rows, staying on a card; the
    /// detail, while open, follows it.
    fn board_move(&mut self, origin: &str, columns: isize, rows: isize) -> Task<Message> {
        let Some((ci, view, cards)) = self.board_cards(origin) else {
            return Task::none();
        };
        let board = &mut self.page.board;
        (board.column, board.row) = board.at(&cards);
        if columns != 0 {
            // Past an empty column to the next one with cards.
            let mut column = board.column as isize;
            loop {
                column += columns;
                if !(0..COLUMNS.len() as isize).contains(&column) {
                    break;
                }
                if !cards[column as usize].is_empty() {
                    board.column = column as usize;
                    break;
                }
            }
        } else if cards[board.column].is_empty()
            && let Some(column) = cards.iter().position(|cards| !cards.is_empty())
        {
            // A cursor in an empty column (a fresh Board, a column the CI
            // emptied) goes to the first card.
            board.column = column;
            board.row = 0;
        }
        let len = cards[board.column].len();
        board.row = if len == 0 {
            0
        } else {
            (board.row as isize + rows).clamp(0, len as isize - 1) as usize
        };
        board.card = cards[board.column].get(board.row).copied();
        let selected = match (board.detail, board.card) {
            (true, Some(Card::Doc(number))) => self.select_doc(&ci, view, number),
            _ => Task::none(),
        };
        Task::batch([selected, self.redock()])
    }

    /// Enter on the Board, and a press on a card: a running card brings its
    /// agent's tab to the front; any other opens the document's detail.
    fn board_enter(&mut self, origin: &str) -> Task<Message> {
        let Some((ci, view, cards)) = self.board_cards(origin) else {
            return Task::none();
        };
        let (column, row) = self.page.board.at(&cards);
        let Some(card) = cards[column].get(row).copied() else {
            return Task::none();
        };
        self.page.board.card = Some(card);
        if column == 3
            && let Some((runner, terminal)) = self.card_terminal(&ci, card)
        {
            return self.apply_page(Message::PageOpen(AttentionTarget::Terminal(
                runner, terminal,
            )));
        }
        let Card::Doc(number) = card else {
            return Task::none();
        };
        self.page.board.detail = true;
        let selected = self.select_doc(&ci, view, number);
        Task::batch([selected, self.redock()])
    }

    /// The runner and terminal of a running card: a plan's master, a plan
    /// session's own.
    fn card_terminal(&self, ci: &RunnerKey, card: Card) -> Option<(RunnerKey, TerminalId)> {
        let client = self.ci.get(ci)?;
        let session = match card {
            Card::Doc(number) => client.sessions().find(|s| {
                s.kind == SessionKind::Master && s.doc == Some(number) && s.state.is_live()
            })?,
            Card::Session(id) => client.sessions().find(|s| s.id == id)?,
        };
        Some((
            client.runner_key(&session.runner)?,
            TerminalId(session.terminal?),
        ))
    }

    /// Notices the documents the CI moved to another column since the last
    /// look, for their ring.
    pub(in crate::app) fn track_board_moves(&mut self) {
        let now = Instant::now();
        let board = &mut self.page.board;
        board
            .moved
            .retain(|_, at| now.duration_since(*at) < MOVED_FOR);
        for client in self.ci.values() {
            for state in client.plans.values() {
                for (group, docs) in doc_groups(state.docs()) {
                    let column = column_of(group);
                    for doc in docs {
                        let key = (doc.repo.clone(), doc.number);
                        if board
                            .columns
                            .insert(key.clone(), column)
                            .is_some_and(|was| was != column)
                        {
                            board.moved.insert(key, now);
                        }
                    }
                }
            }
        }
    }

    /// Whether a moved card's ring is still fading: the frames have to run.
    pub(in crate::app) fn board_animating(&self) -> bool {
        self.page.open
            && matches!(self.page.view, PageView::Board(_))
            && self
                .page
                .board
                .moved
                .values()
                .any(|at| at.elapsed() < MOVED_FOR)
    }

    /// The page: the view it shows.
    pub(in crate::app) fn page_view(&self) -> Element<'_, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let main: Element<'_, Message, Theme> = match &self.page.view {
            PageView::Overview => self.overview_view(&t),
            PageView::Board(origin) => self.board_view(&t, origin),
            PageView::Channel(ci, view) => self.page_channel(ci, *view),
        };
        container(main)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style::solid(t.surface))
            .into()
    }

    /// A caption over a group of rows.
    fn caption<'a>(&self, words: String) -> Element<'a, Message, Theme> {
        container(
            text(words.to_uppercase())
                .size(CAPTION_TEXT)
                .color(self.theme.muted()),
        )
        .padding([0, 8])
        .into()
    }

    /// The Overview: `Needs you`, `Working` and the projects beside the
    /// CIs' runners, under them the terminal of the row under the cursor or
    /// what the chosen runner runs.
    fn overview_view(&self, t: &Tokens) -> Element<'_, Message, Theme> {
        let muted = self.theme.muted();
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
                list =
                    list.push(container(text(empty).size(ROW_TEXT).color(muted)).padding([4, 8]));
            }
            for item in items {
                let active = at == self.page.cursor;
                if active {
                    selected = Some(item.target.clone());
                }
                list = list.push(self.attention_row(t, item, active, at));
                at += 1;
            }
            list = list.push(Space::new().height(8));
        }
        list = list.push(self.projects_list(t, at));

        let detail: Element<'_, Message, Theme> = match (&self.page.machine, selected) {
            (None, Some(AttentionTarget::Terminal(runner, terminal))) => {
                self.docked(t, &runner, terminal, "Bring its tab to the front")
            }
            _ => self.overview_runners(t),
        };
        row![
            scrollable(list).width(LIST_WIDTH).height(Length::Fill),
            rule::vertical(1),
            container(detail).width(Length::Fill).height(Length::Fill),
        ]
        .into()
    }

    /// The projects under their search: name, origin and what it is, each
    /// a row whose press shows its Board; then what the listing could not
    /// reach and `Refresh`.
    fn projects_list<'a>(&'a self, t: &Tokens, first: usize) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut list = column![
            self.caption("Projects".to_owned()),
            container(
                text_input("Search projects", &self.projects.search)
                    .id(PROJECT_SEARCH)
                    .size(ROW_TEXT)
                    .padding([4, 8])
                    .on_input(Message::ProjectSearch),
            )
            .padding([0, 8]),
        ]
        .spacing(6);
        if self.local_runner.is_none() {
            list = list.push(
                container(
                    text("No local runner: only CI repositories are listed.")
                        .size(CAPTION_TEXT)
                        .color(muted),
                )
                .padding([0, 8]),
            );
        } else if self.projects.listing.is_none() && self.projects.asking {
            list = list.push(
                container(
                    text("Asking the local runner...")
                        .size(CAPTION_TEXT)
                        .color(muted),
                )
                .padding([0, 8]),
            );
        }
        for (offset, project) in self.searched_projects().into_iter().enumerate() {
            let active = first + offset == self.page.cursor;
            let mut chips = row![].spacing(4);
            let shown = self
                .projects
                .groups
                .iter()
                .any(|g| g.origin == project.origin);
            for (chip, on) in [
                ("CI", project.ci),
                ("open", shown),
                ("checkout", project.checkout),
            ] {
                if on {
                    chips = chips.push(
                        container(text(chip).size(11))
                            .padding([0, 6])
                            .style(style::badge(t, false)),
                    );
                }
            }
            let line = row![
                column![
                    text(project.name.clone()).size(ROW_TEXT),
                    text(project.origin.clone())
                        .size(CAPTION_TEXT)
                        .color(muted)
                        .wrapping(Wrapping::None),
                ]
                .spacing(2)
                .width(Length::Fill),
                chips,
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            list = list.push(
                container(
                    button(container(line).clip(true))
                        .width(Length::Fill)
                        .padding([4, 8])
                        .style(style::row_entry_button(t, active))
                        .on_press(Message::ShowProject(project.origin)),
                )
                .style(style::row_highlight(t, active)),
            );
        }
        let errors = self
            .projects
            .listing
            .iter()
            .flat_map(|reply| reply.errors.iter())
            .chain(self.projects.listing_error.iter());
        for error in errors {
            list = list.push(
                container(text(error.as_str()).size(CAPTION_TEXT).color(muted)).padding([0, 8]),
            );
        }
        if self.local_runner.is_some() {
            list = list.push(
                container(
                    button(text("Refresh").size(CAPTION_TEXT))
                        .padding([3, 12])
                        .style(style::secondary_button(t))
                        .on_press(Message::RefreshProjects),
                )
                .padding([0, 8]),
            );
        }
        list.into()
    }

    /// Each CI's runners, and what the chosen one runs.
    fn overview_runners(&self, t: &Tokens) -> Element<'_, Message, Theme> {
        let mut tables = column![];
        for (ci, client) in &self.ci {
            let cursor = self
                .page
                .machine
                .as_ref()
                .filter(|(key, _)| key == ci)
                .map(|(_, at)| *at);
            tables = tables.push(self.runners_view(ci, client, cursor));
        }
        let activity: Element<'_, Message, Theme> = match &self.page.machine {
            Some((ci, at)) => match self.ci.get(ci).and_then(|client| {
                let machine = client.overview.as_ref()?.machines.get(*at)?;
                Some((client, machine))
            }) {
                Some((client, machine)) => self.runner_activity(t, client, &machine.name),
                None => Space::new().into(),
            },
            None => container(
                text("Choose a runner to see what it runs.")
                    .size(ROW_TEXT)
                    .color(self.theme.muted()),
            )
            .padding(12)
            .into(),
        };
        column![
            container(tables).height(Length::FillPortion(3)),
            rule::horizontal(1),
            container(activity).height(Length::FillPortion(2)),
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
        at: usize,
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
        let pressed = if active {
            Message::PageOpen(item.target)
        } else {
            Message::PageCursor(at)
        };
        container(
            button(line)
                .width(Length::Fill)
                .padding([6, 8])
                .style(style::row_entry_button(t, active))
                .on_press(pressed),
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

    /// A project's Board: the toolbar, the channel rail, the five columns,
    /// and the selected document's detail beside them.
    fn board_view<'a>(&'a self, t: &Tokens, origin: &'a str) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let Some((ci, client, repo)) = self.ci_of(origin) else {
            return self.unbuilt_board(t, origin);
        };
        let Some((view, state)) = client
            .plans_view_of(&repo.repo)
            .and_then(|view| Some((view, client.plans.get(&view)?)))
        else {
            return unavailable("Board", "The CI has not listed this repository yet.");
        };
        let mut rail = column![].spacing(0).padding([4, 16]);
        let channels = client.channels_of(&repo.repo);
        if channels.is_empty() {
            let line = match repo.onboarding {
                Some(n) => format!("Onboarding requested (#{n}): the CI builds it once approved."),
                None => "No channels yet: the first push starts one.".to_owned(),
            };
            rail = rail.push(
                container(text(line).size(CAPTION_TEXT).color(muted))
                    .height(RAIL_ROW)
                    .align_y(Alignment::Center),
            );
        }
        for channel in channels {
            let tone = Tone::of(&channel.status);
            let mut line = row![
                container(
                    super::glyph_text(channel_glyph(&channel.channel), 14.0)
                        .color(tone.color(&self.theme))
                )
                .width(GLYPH_CELL),
                text(channel.channel.clone()).size(ROW_TEXT).width(96),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .height(RAIL_ROW);
            let view = client.channel_view(&repo.repo, &channel.channel);
            let pipelines = view
                .and_then(|view| client.views.get(&view))
                .map(|state| state.pipelines.as_slice())
                .unwrap_or_default();
            if pipelines.is_empty() {
                line = line.push(text("No pipelines yet").size(CAPTION_TEXT).color(muted));
            }
            let mut dots = row![].spacing(6).align_y(Alignment::Center);
            for shown in pipelines.iter().take(RAIL_PIPELINES) {
                let color = Tone::of(&shown.pipeline.status).color(&self.theme);
                let dot = container(Space::new())
                    .width(PIPELINE_DOT)
                    .height(PIPELINE_DOT)
                    .style(move |_: &Theme| container::Style {
                        background: Some(color.into()),
                        border: iced::border::rounded(PIPELINE_DOT / 2.0),
                        ..container::Style::default()
                    });
                let number = shown.pipeline.number;
                let tip = format!("#{number} {}", shown.pipeline.status);
                let dot = match view {
                    Some(view) => Element::from(
                        mouse_area(dot)
                            .interaction(iced::mouse::Interaction::Pointer)
                            .on_press(Message::PagePipeline(ci.clone(), view, number)),
                    ),
                    None => dot.into(),
                };
                dots = dots.push(super::tip(dot, tip));
            }
            line = line.push(dots);
            rail = rail.push(line);
        }

        let cards = board_columns(
            state.docs(),
            &client
                .sessions()
                .filter(|s| s.repo == repo.repo && s.kind == SessionKind::Plan && s.state.is_live())
                .collect::<Vec<_>>(),
        );
        let cursor = self.page.board.at(&cards);
        let mut columns = row![].spacing(12).padding([8, 16]).height(Length::Fill);
        for (at, (title, cards)) in COLUMNS.iter().zip(cards.iter()).enumerate() {
            let count = container(text(cards.len().to_string()).size(11))
                .padding([0, 6])
                .style(style::badge(t, false));
            let mut list = column![].spacing(8);
            if cards.is_empty() {
                list = list.push(text("Nothing here").size(CAPTION_TEXT).color(muted));
            }
            for (row_at, card) in cards.iter().enumerate() {
                let selected = cursor == (at, row_at);
                list = list.push(self.board_card(
                    t,
                    ci,
                    client,
                    state.docs(),
                    *card,
                    at,
                    row_at,
                    selected,
                ));
            }
            columns = columns.push(
                column![
                    row![
                        text(title.to_uppercase()).size(CAPTION_TEXT).color(muted),
                        count
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                    scrollable(list).height(Length::Fill),
                ]
                .spacing(8)
                .width(Length::FillPortion(1)),
            );
        }
        let board = column![
            self.plans_toolbar(ci, client, view, state),
            rule::horizontal(1),
            rail,
            rule::horizontal(1),
            columns,
        ]
        .width(Length::FillPortion(5))
        .height(Length::Fill);
        if self.page.board.detail && state.selected().is_some() {
            row![
                board,
                rule::vertical(1),
                container(self.doc_detail(ci, client, view, state))
                    .width(Length::FillPortion(4))
                    .height(Length::Fill),
            ]
            .into()
        } else {
            board.into()
        }
    }

    /// One card: number and title, age and `Runs on` chips; a running one
    /// also where it runs and what its agent does.
    #[allow(clippy::too_many_arguments)]
    fn board_card<'a>(
        &'a self,
        t: &Tokens,
        ci: &RunnerKey,
        client: &'a CiClient,
        docs: &'a [DocView],
        card: Card,
        column: usize,
        row_at: usize,
        selected: bool,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let face = match card {
            Card::Doc(number) => match docs.iter().find(|d| d.number == number) {
                Some(doc) => CardFace {
                    head: doc.title.clone(),
                    changed: Some(doc.changed),
                    runs_on: &doc.runs_on,
                    key: Some((doc.repo.clone(), number)),
                },
                None => CardFace::default(),
            },
            Card::Session(id) => {
                let session = client.sessions().find(|s| s.id == id);
                CardFace {
                    head: match session.and_then(|s| s.doc) {
                        Some(n) => format!("plan session #{n}"),
                        None => "plan session".to_owned(),
                    },
                    changed: session.map(|s| s.started),
                    ..CardFace::default()
                }
            }
        };
        let CardFace {
            head,
            changed,
            runs_on,
            key,
        } = face;
        let number = match card {
            Card::Doc(number) => format!("#{number}"),
            Card::Session(_) => String::new(),
        };
        let mut first = row![].spacing(6).align_y(Alignment::Start);
        if !number.is_empty() {
            first = first.push(text(number).size(CAPTION_TEXT).color(muted));
        }
        first = first.push(text(head).size(ROW_TEXT).width(Length::Fill));
        let mut second = row![].spacing(6).align_y(Alignment::Center);
        if let Some(changed) = changed {
            second = second.push(text(age_of(changed)).size(CAPTION_TEXT).color(muted));
        }
        for selector in runs_on {
            second = second.push(
                container(text(selector.as_str()).size(11))
                    .padding([0, 6])
                    .style(style::badge(t, false)),
            );
        }
        if column == 3 {
            second = second.push(self.running_line(ci, card));
        }
        let ring = key
            .and_then(|key| self.page.board.moved.get(&key))
            .map(|at| 1.0 - (at.elapsed().as_secs_f32() / MOVED_FOR.as_secs_f32()).min(1.0))
            .filter(|alpha| *alpha > 0.0);
        let accent = self.theme.accent();
        let surface = t.surface;
        let border = Color { a: 0.3, ..muted };
        let body = container(column![first, second].spacing(4))
            .width(Length::Fill)
            .padding(8)
            .style(move |_: &Theme| {
                let (color, width) = if selected {
                    (accent, 2.0)
                } else if let Some(alpha) = ring {
                    (Color { a: alpha, ..accent }, 2.0)
                } else {
                    (border, 1.0)
                };
                container::Style {
                    background: Some(surface.into()),
                    border: Border {
                        color,
                        width,
                        radius: 6.0.into(),
                    },
                    ..container::Style::default()
                }
            });
        mouse_area(body)
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press(Message::BoardSelect(column, row_at))
            .into()
    }

    /// Where a running card runs and what its agent does: the host glyph,
    /// the role glyph and the state word, `running` without a report.
    fn running_line<'a>(&'a self, ci: &RunnerKey, card: Card) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut line = row![].spacing(4).align_y(Alignment::Center);
        let Some((runner, terminal)) = self.card_terminal(ci, card) else {
            return line
                .push(text("running").size(CAPTION_TEXT).color(muted))
                .into();
        };
        if let Some((glyph, name)) = self.host_of(&runner) {
            line = line.push(super::tip(
                super::glyph_text(glyph, 12.0).color(muted),
                name,
            ));
        }
        let agent = self
            .runtime
            .activity
            .get(&runner)
            .and_then(|entries| entries.iter().find(|e| e.terminal == terminal.0))
            .and_then(|e| e.agent.clone());
        match agent {
            Some(agent) => {
                let color = agent_color(agent.state, &self.theme);
                line = line
                    .push(super::glyph_text(role_glyph(agent.role), 12.0).color(color))
                    .push(
                        text(state_word(agent.state))
                            .size(CAPTION_TEXT)
                            .color(color),
                    );
            }
            None => line = line.push(text("running").size(CAPTION_TEXT).color(muted)),
        }
        line.into()
    }

    /// The Board of a project no CI builds: what it is, and the request
    /// that asks the first CI to build it.
    fn unbuilt_board<'a>(&'a self, t: &Tokens, origin: &'a str) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut body = column![text(format!("{origin} is not built by a CI")).size(ROW_TEXT)]
            .spacing(12)
            .padding(24);
        if self.ci.is_empty() {
            body = body.push(text("No CI configured").size(CAPTION_TEXT).color(muted));
        } else {
            body = body.push(
                button(text("Onboard").size(CAPTION_TEXT))
                    .padding([4, 14])
                    .style(style::pill_button(t))
                    .on_press(Message::OnboardProject(origin.to_owned())),
            );
        }
        body.into()
    }

    /// Asks the first CI to onboard `origin`: a minimal request without
    /// grants or prerequisites, which the owner approves on the Board.
    pub(in crate::app) fn onboard_project(&mut self, origin: String) -> Task<Message> {
        let Some(client) = self.ci.values().next() else {
            return Task::none();
        };
        let name = origin.rsplit('/').next().unwrap_or(&origin).to_owned();
        let default_branch = self
            .listed_projects()
            .into_iter()
            .find(|p| p.origin == origin)
            .map_or_else(|| "main".to_owned(), |p| p.default_branch);
        let request = CiRequest::DocCreate {
            repo: name.clone(),
            kind: DocKind::Onboarding,
            title: format!("Onboard {name}"),
            body: format!("Requested from the editor for {origin}."),
            source: name,
            proposals: Vec::new(),
            runs_on: Vec::new(),
            onboard: Some(OnboardSpec {
                url: format!("https://{origin}"),
                default_branch,
                grants: Default::default(),
                prerequisites: Vec::new(),
            }),
        };
        let endpoint = client.endpoint.clone();
        Task::perform(
            async move { crate::feed::ci(endpoint, request).await.map(|_| ()) },
            Message::Onboarded,
        )
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
        let open = self.page.open && self.page.view == PageView::Overview;
        let chip = if open {
            chip.style(style::pill_button(&t))
        } else {
            chip.style(style::secondary_button(&t))
        };
        Some(chip.into())
    }
}

/// What a session's row opens: its terminal while one is known, else its
/// repository's Board.
fn session_target(client: &CiClient, session: &SessionView) -> AttentionTarget {
    match (client.runner_key(&session.runner), session.terminal) {
        (Some(runner), Some(terminal)) => AttentionTarget::Terminal(runner, TerminalId(terminal)),
        _ => AttentionTarget::Page(PageView::Board(origin_of(client, &session.repo))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::DocState;

    fn doc(number: u64, kind: DocKind, state: DocState, changed: u64) -> DocView {
        DocView {
            repo: "r".to_owned(),
            number,
            kind,
            title: format!("d{number}"),
            state,
            source: "r".to_owned(),
            created: 0,
            changed,
            approved: None,
            proposals: Vec::new(),
            plan: None,
            runs_on: Vec::new(),
            placement: Vec::new(),
            body: String::new(),
            onboard: None,
            note: String::new(),
        }
    }

    #[test]
    fn documents_fall_into_the_lifecycle_columns_and_done_keeps_the_newest() {
        let mut docs = vec![
            doc(1, DocKind::Proposal, DocState::Open, 1),
            doc(2, DocKind::Plan, DocState::Saved, 1),
            doc(3, DocKind::Plan, DocState::Approved, 1),
            doc(4, DocKind::Plan, DocState::InProgress, 1),
        ];
        docs.extend((10..30).map(|n| doc(n, DocKind::Plan, DocState::Done, n)));
        let session = SessionView {
            id: 7,
            repo: "r".to_owned(),
            kind: SessionKind::Plan,
            doc: None,
            runner: "a".to_owned(),
            machines: Vec::new(),
            state: SessionState::Running,
            terminal: Some(3),
            started: 0,
            ended: None,
            code: None,
            message: String::new(),
        };
        let columns = board_columns(&docs, &[&session]);
        assert_eq!(columns[0], [Card::Doc(1)]);
        assert_eq!(columns[1], [Card::Doc(2)]);
        assert_eq!(columns[2], [Card::Doc(3)]);
        assert_eq!(columns[3], [Card::Doc(4), Card::Session(7)]);
        assert_eq!(columns[4].len(), DONE_SHOWN);
        assert_eq!(columns[4][0], Card::Doc(29));
    }
}
