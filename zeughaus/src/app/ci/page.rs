//! The CI views: the Overview the titlebar chip opens, a project's Board, a
//! channel's pipelines and a job's transcript, the last three as tabs of the
//! project's group in the bar (a job's right after its channel's). The
//! terminal of an Overview row or of a Board's detail is docked read-only
//! beside it.
//!
//! The Board is a project's lifecycle on one screen, read left to right:
//! tickets are proposed, planned, reviewed, queued, run and finish, with the
//! channels' health above. Two lanes appear between the columns while
//! something passes through them: a ticket a session plans, a master that
//! starts. A ticket opens in a popout over the workspace (`ticket.rs`).
//! What runs or waits pulses; a card the CI moves to another slot arrives
//! with an accent ring that fades over [`MOVED_FOR`].

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iced::widget::text::Wrapping;
use iced::widget::{
    Space, button, column, container, mouse_area, row, rule, stack, text, text_input,
};
use iced::{Alignment, Border, Color, Element, Length, Task};
use iced_tabs::scroll;
use zeughaus_link::{
    ActivityKind, ActivityState, AgentRole, AgentState, ClaimSubject, ClaimView, DocGroup,
    DocState, DocView, RepoDocsView, SessionKind, SessionState, SessionView, doc_groups,
};
use zeughaus_mux::TerminalId;
use zeughaus_theme::Theme;

use super::{CiClient, JobRef, Tone, glyph, now, unavailable};
use crate::app::App;
use crate::app::agents::{agent_color, attention_title, role_glyph, role_word, state_word};
use crate::message::{CiShortcut, Message, TicketMsg};
use crate::notices::{self, Severity};
use crate::style::{self, Tokens};
use crate::workspace::RunnerKey;

/// The width of the Overview's list.
const LIST_WIDTH: f32 = 420.0;
/// The column a row's glyph stands in, so the words after it align.
const GLYPH_CELL: f32 = 18.0;
/// Text of a row, and of a caption.
const ROW_TEXT: f32 = 13.0;
const CAPTION_TEXT: f32 = 12.0;
/// A session that failed longer ago than this needs nobody any more.
const FAILED_SESSION_RECENT: Duration = Duration::from_secs(24 * 3600);
/// How long before a CI's Claude sign-in ends the Overview asks for a new
/// one.
const CLAUDE_WARN: Duration = Duration::from_secs(3 * 24 * 3600);
/// The Board's rail: a row per channel and the pipelines each shows.
const RAIL_ROW: f32 = 28.0;
const RAIL_PIPELINES: usize = 12;
const PIPELINE_DOT: f32 = 8.0;
/// The finished documents the DONE slot shows.
const DONE_SHOWN: usize = 12;
/// How long a card the CI moved carries its ring.
const MOVED_FOR: Duration = Duration::from_secs(2);
/// The narrowest a Board column gets before the board scrolls sideways;
/// a lane is always this wide.
const COLUMN_MIN: f32 = 168.0;

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
    /// A job's transcript, by its channel's view, pipeline and name.
    Job(RunnerKey, JobRef),
}

/// What a row of the Overview opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttentionTarget {
    /// The tab of a runner's terminal.
    Terminal(RunnerKey, TerminalId),
    /// A view of the page.
    Page(PageView),
    /// The local sign-in of a CI's Claude subscription.
    ClaudeSignIn(RunnerKey),
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
    /// The Boards of projects no CI builds, by origin.
    pub(in crate::app) onboard: super::onboard::Drafts,
}

impl Default for Page {
    fn default() -> Page {
        Page {
            open: false,
            view: PageView::Overview,
            cursor: 0,
            machine: None,
            board: Board::default(),
            onboard: Default::default(),
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
    /// The slot each document was last seen in, by repository and number.
    columns: HashMap<(String, u64), usize>,
    /// When the CI moved a document to another slot.
    moved: HashMap<(String, u64), Instant>,
    /// The card the cursor is on: the cursor follows it to another slot
    /// when the CI moves it.
    card: Option<Card>,
}

impl Board {
    /// Where the cursor is: on its card wherever that is now, else at its
    /// last place.
    fn at(&self, cards: &Slots) -> (usize, usize) {
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
    /// A plan session this editor asked for, which has no session yet.
    Opening,
}

/// The slots of the Board, left to right: a title, and whether it is a
/// lane, drawn narrow and only while a card passes through it.
const SLOTS: [(&str, bool); 7] = [
    ("Proposals", false),
    ("Planning", true),
    ("Review", false),
    ("Queue", false),
    ("Starting", true),
    ("Running", false),
    ("Done", false),
];
const PROPOSALS: usize = 0;
const PLANNING: usize = 1;
const PLANS: usize = 2;
const QUEUE: usize = 3;
const STARTING: usize = 4;
const RUNNING: usize = 5;
const DONE: usize = 6;

/// A repository's cards in their slots.
type Slots = [Vec<Card>; 7];

/// Whether a card in slot `at` is work a session does: it opens that
/// session's tab.
fn active(at: usize) -> bool {
    matches!(at, PLANNING | STARTING | RUNNING)
}

/// Seconds since the first frame that asked: the clock every pulse and
/// flow of the page runs on.
pub(super) fn phase() -> f32 {
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
    EPOCH.elapsed().as_secs_f32()
}

/// 0 to 1 and back over 1.6 s: what waits breathes.
pub(super) fn pulse(phase: f32) -> f32 {
    0.5 + 0.5 * (phase * TAU / 1.6).sin()
}

/// 0 to 1 over 1.4 s, then again: what moves flows.
fn flow(phase: f32) -> f32 {
    (phase / 1.4).fract()
}

/// `color` at alpha `a`.
pub(super) fn faded(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

/// A repository's cards in their slots: its tickets, its live plan
/// sessions and a free plan session asked for (`opening`). An open ticket a
/// session holds the claim on, and a ready or planning ticket, is in the
/// PLANNING lane, except a ready one that waits for its dependencies, which
/// stays with the proposals; a ticket whose master starts is in the
/// STARTING lane. A ticket merged into one that is not finished is left
/// out: the including ticket's card names it. `sessions` and `claims` are
/// the repository's.
fn board_columns(
    docs: &[DocView],
    sessions: &[&SessionView],
    claims: &[&ClaimView],
    opening: bool,
) -> Slots {
    let mut slots: Slots = Default::default();
    let claimed = |number: u64| {
        claims.iter().any(
            |claim| matches!(claim.subject, ClaimSubject::Ticket { number: n, .. } if n == number),
        )
    };
    let live = |kind: SessionKind, number: u64| {
        sessions
            .iter()
            .find(|s| s.kind == kind && s.doc == Some(number) && s.state.is_live())
    };
    let finished = |number: u64| {
        docs.iter()
            .find(|doc| doc.number == number)
            .is_none_or(|doc| matches!(doc.state, DocState::Done | DocState::Dismissed))
    };
    let mut done = 0;
    for (group, members) in doc_groups(docs) {
        for doc in members {
            let number = doc.number;
            let slot = match group {
                DocGroup::Open if claimed(number) => PLANNING,
                DocGroup::Open => PROPOSALS,
                DocGroup::Planning if doc.state == DocState::Ready && !doc.waits_for.is_empty() => {
                    PROPOSALS
                }
                DocGroup::Planning => PLANNING,
                DocGroup::Review => PLANS,
                DocGroup::Queue => QUEUE,
                DocGroup::InProgress => match live(SessionKind::Master, number) {
                    Some(master) if master.state == SessionState::Starting => STARTING,
                    _ => RUNNING,
                },
                DocGroup::History
                    if doc.state == DocState::Merged && !doc.into.is_none_or(finished) =>
                {
                    continue;
                }
                DocGroup::History => DONE,
            };
            if slot == DONE {
                if done == DONE_SHOWN {
                    continue;
                }
                done += 1;
            }
            slots[slot].push(Card::Doc(number));
        }
    }
    // A plan session whose proposal has no card in the lane has one of its
    // own there.
    for session in sessions
        .iter()
        .filter(|s| s.kind == SessionKind::Plan && s.state.is_live())
    {
        if !session
            .doc
            .is_some_and(|n| slots[PLANNING].contains(&Card::Doc(n)))
        {
            slots[PLANNING].push(Card::Session(session.id));
        }
    }
    if opening {
        slots[PLANNING].push(Card::Opening);
    }
    slots
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
    pub(super) fn ci_of(&self, origin: &str) -> Option<(&RunnerKey, &CiClient, &RepoDocsView)> {
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
            if let Some(item) = claude_item(ci, overview.claude.as_ref(), now) {
                items.push(item);
            }
            for summary in overview.repos.iter().filter(|r| r.awaiting > 0) {
                let Some(repo) = client.repos.get(&summary.repo) else {
                    continue;
                };
                let n = summary.awaiting;
                items.push(AttentionItem {
                    severity: Severity::Info,
                    glyph: glyph::PASS,
                    title: format!(
                        "{}: {n} ticket{} await{} review",
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
            PageView::Board(_) | PageView::Channel(..) | PageView::Job(..) => None,
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
                self.ticket = None;
                let asked = match &view {
                    PageView::Overview => self.ask_projects(false),
                    PageView::Board(origin) => {
                        // A Board is a tab of its project's group.
                        self.keep_project(origin);
                        if self.page.board.origin != *origin {
                            self.page.board.origin.clone_from(origin);
                            self.page.board.column = 0;
                            self.page.board.row = 0;
                            self.page.board.card = None;
                        }
                        // A project no CI builds: what the CI advises.
                        self.ensure_advice(origin)
                    }
                    PageView::Channel(ci, channel) => {
                        self.adopt_channel_view(ci, *channel);
                        Task::none()
                    }
                    PageView::Job(ci, job) => {
                        self.adopt_job_view(ci, job);
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
                PageView::Job(..) => {}
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
                PageView::Job(..) => {}
            },
            Message::PageOpen(AttentionTarget::Terminal(runner, terminal)) => {
                self.page.open = false;
                let revealed = self.apply(Message::RevealTerminal(runner, terminal));
                return Task::batch([revealed, self.redock()]);
            }
            Message::PageOpen(AttentionTarget::Page(view)) => {
                return self.apply_page(Message::PageShow(view));
            }
            Message::PageOpen(AttentionTarget::ClaudeSignIn(ci)) => {
                return self.claude_sign_in(&ci);
            }
            Message::ClaudeSignedIn(result) => {
                let (severity, title, body) = match result {
                    Ok(line) => (Severity::Info, "Claude signed in", line),
                    Err(e) => (Severity::Error, "Claude sign-in failed", e),
                };
                self.notices
                    .raise(None, severity, title, body, false, None, SystemTime::now());
                return self.poll_ci();
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
    /// the arrows) change the column and `a`, `r` and `p` approve, dismiss
    /// and release the ticket under the cursor, or the one an open popout
    /// shows. `None` for a letter the page leaves to the channel in front.
    pub(in crate::app) fn page_shortcut(&mut self, shortcut: CiShortcut) -> Option<Task<Message>> {
        if let Some(popout) = &self.ticket {
            let (ci, view, number) = (popout.ci.clone(), popout.view, popout.number);
            return Some(match (shortcut, number) {
                (CiShortcut::Approve | CiShortcut::Rerun | CiShortcut::Plan, Some(number)) => {
                    self.plans_letter(&ci, view, number, shortcut)
                }
                _ => Task::none(),
            });
        }
        if !self.page.open {
            return None;
        }
        let rows = match shortcut {
            CiShortcut::Next => Some(1),
            CiShortcut::Previous => Some(-1),
            _ => None,
        };
        match self.page.view.clone() {
            PageView::Job(..) => Some(Task::none()),
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
                        Some((ci, view, number)) => self.plans_letter(&ci, view, number, shortcut),
                        None => Task::none(),
                    }
                }
                CiShortcut::Cancel => Task::none(),
            }),
        }
    }

    /// Signs CI `ci` in to Claude through this machine's browser: runs
    /// `zeughaus-runner claude login` (the one beside this editor, else the
    /// one on `PATH`) against that CI, and says how it went as a notice.
    fn claude_sign_in(&mut self, ci: &RunnerKey) -> Task<Message> {
        let Some(client) = self.ci.get(ci) else {
            return Task::none();
        };
        let url = client.endpoint.0.clone();
        let runner = std::env::current_exe()
            .ok()
            .map(|exe| {
                exe.with_file_name(if cfg!(windows) {
                    "zeughaus-runner.exe"
                } else {
                    "zeughaus-runner"
                })
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| "zeughaus-runner".into());
        let state_dir = zeughaus_link::credentials::state_dir();
        let (sender, answer) = iced::futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let output = std::process::Command::new(&runner)
                .arg("--state-dir")
                .arg(&state_dir)
                .args(["claude", "login", "--ci", &url])
                .stdin(std::process::Stdio::null())
                .output();
            let result = match output {
                Ok(output) if output.status.success() => {
                    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
                }
                Ok(output) => Err(String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .rfind(|line| !line.trim().is_empty())
                    .unwrap_or("zeughaus-runner claude login failed")
                    .trim_start_matches("[claude] ")
                    .to_owned()),
                Err(e) => Err(format!("cannot run {}: {e}", runner.display())),
            };
            let _ = sender.send(result);
        });
        Task::perform(answer, |result| {
            Message::ClaudeSignedIn(result.unwrap_or_else(|_| Err("the sign-in ended".to_owned())))
        })
    }

    /// What Escape does on the page when nothing nearer took it: closes the
    /// Overview.
    pub(in crate::app) fn page_escape(&mut self) -> Task<Message> {
        if !self.page.open {
            return Task::none();
        }
        match self.page.view.clone() {
            PageView::Overview => {
                self.page.open = false;
                self.redock()
            }
            PageView::Board(_) | PageView::Channel(..) | PageView::Job(..) => Task::none(),
        }
    }

    /// The cards of `origin`'s Board and where they come from: the CI, the
    /// Plans view and the repository.
    fn board_cards(&self, origin: &str) -> Option<(RunnerKey, u64, String, Slots)> {
        let (ci, client, repo) = self.ci_of(origin)?;
        let view = client.plans_view_of(&repo.repo)?;
        let state = client.plans.get(&view)?;
        let slots = self.repo_slots(ci, client, &repo.repo, state.docs());
        Some((ci.clone(), view, repo.repo.clone(), slots))
    }

    /// `repo`'s cards of CI `ci` in their slots.
    fn repo_slots(&self, ci: &RunnerKey, client: &CiClient, repo: &str, docs: &[DocView]) -> Slots {
        let sessions: Vec<&SessionView> = client.sessions().filter(|s| s.repo == repo).collect();
        let claims: Vec<&ClaimView> = client
            .claims()
            .filter(|claim| claim.subject.repo() == repo)
            .collect();
        board_columns(docs, &sessions, &claims, self.opening_on(ci, repo))
    }

    /// Whether this editor asked CI `ci` for a plan session of `repo`
    /// without a ticket, which has no session yet.
    fn opening_on(&self, ci: &RunnerKey, repo: &str) -> bool {
        self.opening_session
            .as_ref()
            .is_some_and(|(key, asked)| key == ci && asked == repo)
    }

    /// The document under the Board's cursor, with its CI and Plans view.
    fn board_doc(&self, origin: &str) -> Option<(RunnerKey, u64, u64)> {
        let (ci, view, _, slots) = self.board_cards(origin)?;
        let (column, row) = self.page.board.at(&slots);
        match slots[column].get(row)? {
            Card::Doc(number) => Some((ci, view, *number)),
            Card::Session(_) | Card::Opening => None,
        }
    }

    /// Moves the Board's cursor by columns and rows, staying on a card.
    fn board_move(&mut self, origin: &str, columns: isize, rows: isize) -> Task<Message> {
        let Some((_, _, _, cards)) = self.board_cards(origin) else {
            return Task::none();
        };
        let board = &mut self.page.board;
        (board.column, board.row) = board.at(&cards);
        if columns != 0 {
            // Past an empty slot (an empty lane is not drawn) to the next
            // one with cards.
            let mut column = board.column as isize;
            loop {
                column += columns;
                if !(0..SLOTS.len() as isize).contains(&column) {
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
        self.redock()
    }

    /// Enter on the Board, and a press on a card: a card of work a session
    /// does brings that session's tab to the front; any other opens the
    /// ticket's popout.
    fn board_enter(&mut self, origin: &str) -> Task<Message> {
        let Some((ci, view, repo, cards)) = self.board_cards(origin) else {
            return Task::none();
        };
        let (column, row) = self.page.board.at(&cards);
        let Some(card) = cards[column].get(row).copied() else {
            return Task::none();
        };
        self.page.board.card = Some(card);
        if active(column)
            && let Some((runner, terminal)) = self.card_holder(&ci, &repo, card)
        {
            return self.apply_page(Message::PageOpen(AttentionTarget::Terminal(
                runner, terminal,
            )));
        }
        let Card::Doc(number) = card else {
            return Task::none();
        };
        self.apply_ticket(TicketMsg::Open { ci, view, number })
    }

    /// The live session of a card: a ticket's master or plan session, a
    /// plan session itself.
    fn card_session(&self, ci: &RunnerKey, repo: &str, card: Card) -> Option<&SessionView> {
        let client = self.ci.get(ci)?;
        match card {
            Card::Doc(number) => client
                .sessions()
                .find(|s| s.repo == repo && s.doc == Some(number) && s.state.is_live()),
            Card::Session(id) => client.sessions().find(|s| s.id == id),
            Card::Opening => None,
        }
    }

    /// The runner and terminal responsible for a card: its session's, else
    /// the holder of the claim on its ticket.
    fn card_holder(
        &self,
        ci: &RunnerKey,
        repo: &str,
        card: Card,
    ) -> Option<(RunnerKey, TerminalId)> {
        let client = self.ci.get(ci)?;
        if let Some(session) = self.card_session(ci, repo, card)
            && let (Some(runner), Some(terminal)) =
                (client.runner_key(&session.runner), session.terminal)
        {
            return Some((runner, TerminalId(terminal)));
        }
        let Card::Doc(number) = card else {
            return None;
        };
        client.holder_target(client.claim_on(&ClaimSubject::Ticket {
            repo: repo.to_owned(),
            number,
        })?)
    }

    /// Notices the documents the CI moved to another slot since the last
    /// look, for their ring.
    pub(in crate::app) fn track_board_moves(&mut self) {
        let now = Instant::now();
        let mut seen = Vec::new();
        for (ci, client) in &self.ci {
            for state in client.plans.values() {
                let slots = self.repo_slots(ci, client, &state.repo, state.docs());
                for (slot, cards) in slots.iter().enumerate() {
                    for card in cards {
                        if let Card::Doc(number) = card {
                            seen.push(((state.repo.clone(), *number), slot));
                        }
                    }
                }
            }
        }
        let board = &mut self.page.board;
        board
            .moved
            .retain(|_, at| now.duration_since(*at) < MOVED_FOR);
        for (key, slot) in seen {
            if board
                .columns
                .insert(key.clone(), slot)
                .is_some_and(|was| was != slot)
            {
                board.moved.insert(key, now);
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

    /// Whether the page shows something that runs or waits, which pulses: a
    /// card in a lane, an action or a session on its way, a running or
    /// queued pipeline.
    pub(in crate::app) fn page_pulsing(&self) -> bool {
        if !self.page.open {
            return false;
        }
        match &self.page.view {
            PageView::Overview => false,
            PageView::Board(origin) => {
                let Some((ci, client, repo)) = self.ci_of(origin) else {
                    return false;
                };
                let Some(state) = client
                    .plans_view_of(&repo.repo)
                    .and_then(|view| client.plans.get(&view))
                else {
                    return false;
                };
                let slots = self.repo_slots(ci, client, &repo.repo, state.docs());
                !slots[PLANNING].is_empty()
                    || !slots[STARTING].is_empty()
                    || state.acting_on.is_some()
                    || client
                        .channels_of(&repo.repo)
                        .iter()
                        .any(|channel| channel.status == "running")
                    || client
                        .overview
                        .iter()
                        .flat_map(|overview| &overview.queued)
                        .any(|queued| queued.repo == repo.repo)
            }
            PageView::Channel(ci, view) => self
                .ci
                .get(ci)
                .and_then(|client| client.views.get(view))
                .is_some_and(|state| {
                    state
                        .pipelines
                        .iter()
                        .any(|shown| shown.pipeline.status == "running")
                }),
            PageView::Job(..) => false,
        }
    }

    /// The page: the view it shows.
    pub(in crate::app) fn page_view(&self) -> Element<'_, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let main: Element<'_, Message, Theme> = match &self.page.view {
            PageView::Overview => self.overview_view(&t),
            PageView::Board(origin) => self.board_view(&t, origin),
            PageView::Channel(ci, view) => self.page_channel(ci, *view),
            PageView::Job(ci, job) => self.job_page(ci, job),
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
            scroll::vertical(list)
                .width(LIST_WIDTH)
                .height(Length::Fill),
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
        // What a runner runs takes the lower part once one is chosen; until
        // then the tables keep the height and a line says how.
        let chosen = self.page.machine.as_ref().and_then(|(ci, at)| {
            let client = self.ci.get(ci)?;
            let machine = client.overview.as_ref()?.machines.get(*at)?;
            Some((client, machine))
        });
        match chosen {
            Some((client, machine)) => column![
                container(tables).height(Length::FillPortion(3)),
                rule::horizontal(1),
                container(self.runner_activity(t, client, &machine.name))
                    .height(Length::FillPortion(2)),
            ]
            .into(),
            None => column![
                container(tables).height(Length::Fill),
                container(
                    text("Choose a runner to see what it runs.")
                        .size(CAPTION_TEXT)
                        .color(self.theme.muted()),
                )
                .padding([8, 16]),
            ]
            .into(),
        }
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

    /// A channel: its toolbar over its pipelines.
    fn page_channel<'a>(&'a self, ci: &RunnerKey, view: u64) -> Element<'a, Message, Theme> {
        let Some((ci, client)) = self.ci.get_key_value(ci) else {
            return unavailable("CI", "This CI is not connected.");
        };
        let Some(state) = client.views.get(&view) else {
            return unavailable("CI", "This channel is gone.");
        };
        self.channel_view(ci, client, view, state)
    }

    /// A project's Board: the toolbar, the channel rail and the columns
    /// with the lanes between them.
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
        let p = phase();
        let rail = self.board_rail(ci, client, repo, p);
        let cards = self.repo_slots(ci, client, &repo.repo, state.docs());
        let cursor = self.page.board.at(&cards);
        // Every column, and a lane while a card passes through it.
        let shown: Vec<usize> = (0..SLOTS.len())
            .filter(|&at| !SLOTS[at].1 || !cards[at].is_empty())
            .collect();
        let accent = self.theme.accent();
        // Equal columns, each at least `COLUMN_MIN` wide: below that the
        // columns keep their width and the board scrolls sideways. A lane
        // is always that narrow.
        let t = *t;
        let columns = iced::widget::responsive(move |size| {
            let narrow = size.width < shown.len() as f32 * (COLUMN_MIN + 12.0) + 32.0;
            let mut columns = row![].spacing(12).padding([8, 16]).height(Length::Fill);
            for &at in &shown {
                let (title, lane) = SLOTS[at];
                let slot = &cards[at];
                let width = if lane || narrow {
                    Length::Fixed(COLUMN_MIN)
                } else {
                    Length::FillPortion(1)
                };
                let count = container(text(slot.len().to_string()).size(11))
                    .padding([0, 6])
                    .style(style::badge(&t, false));
                let mut head = row![].spacing(6).align_y(Alignment::Center);
                if lane {
                    head = head.push(super::glyph_text(glyph::CHEVRON_RIGHT, 12.0).color(accent));
                }
                head = head
                    .push(text(title.to_uppercase()).size(CAPTION_TEXT).color(muted))
                    .push(count);
                let mut list = column![].spacing(6);
                if slot.is_empty() {
                    list = list.push(text("Nothing here").size(CAPTION_TEXT).color(muted));
                }
                for (row_at, card) in slot.iter().enumerate() {
                    list = list.push(self.board_card(
                        &t,
                        ci,
                        client,
                        repo,
                        state,
                        *card,
                        (at, row_at),
                        cursor == (at, row_at),
                        p,
                    ));
                }
                let list = scroll::vertical(list).height(Length::Fill);
                let body: Element<'_, Message, Theme> = if lane {
                    // A guide down the lane's middle, breathing.
                    let guide = faded(accent, 0.25 + 0.35 * pulse(p));
                    let line = container(Space::new()).width(2).height(Length::Fill).style(
                        move |_: &Theme| container::Style {
                            background: Some(guide.into()),
                            ..container::Style::default()
                        },
                    );
                    stack![
                        container(line).center_x(Length::Fill).height(Length::Fill),
                        list,
                    ]
                    .into()
                } else {
                    list.into()
                };
                columns = columns.push(column![head, body].spacing(8).width(width));
            }
            if narrow {
                scroll::horizontal(columns).height(Length::Fill).into()
            } else {
                columns.into()
            }
        });
        column![
            self.plans_toolbar(ci, client, view, state),
            rule::horizontal(1),
            rail,
            rule::horizontal(1),
            columns,
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// The rail over a Board: each channel with its newest pipelines, led
    /// by the events that wait for one as hollow dots. A running pipeline
    /// pulses; a held one wears a ring in its holder's colour.
    fn board_rail<'a>(
        &'a self,
        ci: &'a RunnerKey,
        client: &'a CiClient,
        repo: &'a RepoDocsView,
        p: f32,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut rail = column![].spacing(0).padding([4, 16]);
        let channels = client.channels_of(&repo.repo);
        if channels.is_empty() {
            let line = match repo.onboarding {
                Some(n) => {
                    format!("Onboarding #{n}: the CI joins the repository, then its tickets start.")
                }
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
            let mut dots = row![].spacing(6).align_y(Alignment::Center);
            let mut queued = 0;
            for waiting in client
                .overview
                .iter()
                .flat_map(|overview| &overview.queued)
                .filter(|q| q.repo == repo.repo && q.channel == channel.channel)
            {
                queued += 1;
                let color = faded(self.theme.accent(), 0.3 + 0.7 * pulse(p));
                let dot = container(Space::new())
                    .width(PIPELINE_DOT)
                    .height(PIPELINE_DOT)
                    .style(move |_: &Theme| container::Style {
                        border: Border {
                            color,
                            width: 1.5,
                            radius: (PIPELINE_DOT / 2.0).into(),
                        },
                        ..container::Style::default()
                    });
                dots = dots.push(super::tip(
                    dot,
                    format!("queued {} {}", waiting.event, waiting.git_ref),
                ));
            }
            if pipelines.is_empty() && queued == 0 {
                line = line.push(text("No pipelines yet").size(CAPTION_TEXT).color(muted));
            }
            for shown in pipelines.iter().take(RAIL_PIPELINES) {
                let mut color = Tone::of(&shown.pipeline.status).color(&self.theme);
                if shown.pipeline.status == "running" {
                    color = faded(color, 0.35 + 0.65 * pulse(p));
                }
                let number = shown.pipeline.number;
                let mut tip = format!("#{number} {}", shown.pipeline.status);
                let held = client.claim_on(&ClaimSubject::Pipeline {
                    repo: repo.repo.clone(),
                    number,
                });
                let ring = held.map(|claim| {
                    tip.push_str(&format!("\nheld by {}", claim.holder_text()));
                    self.holder_color(client, claim)
                });
                // A ring around the dot, not over it.
                let size = PIPELINE_DOT + if ring.is_some() { 4.0 } else { 0.0 };
                let dot =
                    container(Space::new())
                        .width(size)
                        .height(size)
                        .style(move |_: &Theme| container::Style {
                            background: Some(color.into()),
                            border: Border {
                                color: ring.unwrap_or(Color::TRANSPARENT),
                                width: if ring.is_some() { 2.0 } else { 0.0 },
                                radius: (size / 2.0).into(),
                            },
                            ..container::Style::default()
                        });
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
        rail.into()
    }

    /// What the agent in `runner`'s terminal reports, once it reports.
    pub(super) fn agent_at(
        &self,
        runner: &RunnerKey,
        terminal: TerminalId,
    ) -> Option<&zeughaus_link::AgentStatus> {
        self.runtime
            .activity
            .get(runner)?
            .iter()
            .find(|entry| entry.terminal == terminal.0)?
            .agent
            .as_ref()
    }

    /// The colour of a claim's holder: its agent's state, else the accent.
    pub(super) fn holder_color(&self, client: &CiClient, claim: &ClaimView) -> Color {
        client
            .holder_target(claim)
            .and_then(|(runner, terminal)| self.agent_at(&runner, terminal))
            .map_or(self.theme.accent(), |agent| {
                agent_color(agent.state, &self.theme)
            })
    }

    /// One card: number and title, age, `Runs on` chips, the tickets it
    /// includes, what it waits for and its plan's revision; in a lane or
    /// RUNNING also who works on it, what is being done to it, what its
    /// master waits on. A card in a lane carries a flow bar.
    #[allow(clippy::too_many_arguments)]
    fn board_card<'a>(
        &'a self,
        t: &Tokens,
        ci: &RunnerKey,
        client: &'a CiClient,
        repo: &'a RepoDocsView,
        state: &'a super::plans::PlansState,
        card: Card,
        (column, row_at): (usize, usize),
        selected: bool,
        p: f32,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let accent = self.theme.accent();
        let doc = match card {
            Card::Doc(number) => state.docs().iter().find(|d| d.number == number),
            Card::Session(_) | Card::Opening => None,
        };
        let session = self.card_session(ci, &repo.repo, card);
        let head = match card {
            Card::Doc(_) => doc.map(|d| d.title.clone()).unwrap_or_default(),
            Card::Session(_) => match session.and_then(|s| s.doc) {
                Some(n) => format!("plan session #{n}"),
                None => "plan session".to_owned(),
            },
            Card::Opening => "plan session".to_owned(),
        };
        let changed = match card {
            Card::Doc(_) => doc.map(|d| d.changed),
            Card::Session(_) => session.map(|s| s.started),
            Card::Opening => None,
        };
        let mut first = row![].spacing(6).align_y(Alignment::Start);
        if let Card::Doc(number) = card {
            first = first.push(text(format!("#{number}")).size(CAPTION_TEXT).color(muted));
        }
        first = first.push(text(head).size(ROW_TEXT).width(Length::Fill));
        let mut second = row![].spacing(6).align_y(Alignment::Center);
        // Work a session does counts from the session's start, the rest
        // from their last change.
        let since = match session {
            Some(session) if active(column) => Some(session.started),
            _ => changed,
        };
        if let Some(since) = since {
            second = second.push(text(age_of(since)).size(CAPTION_TEXT).color(muted));
        }
        let chip = |words: String| {
            container(text(words).size(11))
                .padding([0, 6])
                .style(style::badge(t, false))
        };
        if let Some(doc) = doc {
            for selector in &doc.runs_on {
                second = second.push(chip(selector.clone()));
            }
            for number in &doc.merged {
                second = second.push(chip(format!("#{number}")));
            }
            if matches!(
                doc.state,
                DocState::Ready | DocState::Approved | DocState::Open
            ) {
                for link in &doc.waits_for {
                    second = second.push(
                        text(format!("after {}", link.reference(&doc.repo)))
                            .size(11)
                            .color(muted),
                    );
                }
            }
            if doc.revision > 0 {
                second = second.push(
                    text(format!("plan v{}", doc.revision))
                        .size(11)
                        .color(muted),
                );
            }
            // A ticket whose plan needs no review, while it still has one
            // to skip.
            if doc.auto_approve
                && matches!(
                    doc.state,
                    DocState::Open | DocState::Ready | DocState::Planning
                )
            {
                second = second.push(chip("runs through".to_owned()));
            }
        }
        let mut lines = column![first, second].spacing(4);
        if active(column) {
            let opening = card == Card::Opening;
            lines = lines.push(match self.card_holder(ci, &repo.repo, card) {
                Some((runner, terminal)) => self.running_line(runner, terminal),
                None => text(if opening {
                    "requesting session"
                } else {
                    "no session"
                })
                .size(CAPTION_TEXT)
                .color(muted)
                .into(),
            });
        }
        let acting = match (state.acting_on, card) {
            (Some((number, verb)), Card::Doc(on)) if number == on => Some(verb),
            _ => None,
        };
        if let Some(verb) = acting {
            lines = lines.push(
                text(format!("{verb}\u{2026}"))
                    .size(CAPTION_TEXT)
                    .color(accent),
            );
        }
        if column == QUEUE && repo.paused {
            lines = lines.push(text("queue paused").size(CAPTION_TEXT).color(muted));
        }
        if column == RUNNING
            && let Some(master) = session.filter(|s| s.kind == SessionKind::Master)
        {
            for claim in client.claims().filter(|claim| {
                claim.holder.session == Some(master.id)
                    && matches!(claim.subject, ClaimSubject::Pipeline { .. })
            }) {
                lines = lines.push(self.wait_line(client, claim, p));
            }
        }
        let lane = SLOTS[column].1;
        if lane {
            // A bar that flows along the card: the work moves on.
            let at = (flow(p) * 80.0).round() as u16;
            lines = lines.push(
                row![
                    Space::new().width(Length::FillPortion(1 + at)),
                    container(Space::new())
                        .width(Length::FillPortion(20))
                        .height(3)
                        .style(move |_: &Theme| container::Style {
                            background: Some(accent.into()),
                            border: iced::border::rounded(1.5),
                            ..container::Style::default()
                        }),
                    Space::new().width(Length::FillPortion(1 + 80 - at)),
                ]
                .height(3),
            );
        }
        // The ring of a card the CI just moved eases out around it, outside
        // the border that marks the selection.
        let ring = doc
            .and_then(|doc| self.page.board.moved.get(&(doc.repo.clone(), doc.number)))
            .map(|at| (1.0 - at.elapsed().as_secs_f32() / MOVED_FOR.as_secs_f32()).max(0.0))
            .map(|left| left * left)
            .filter(|alpha| *alpha > 0.0);
        let surface = t.surface;
        // What runs or waits breathes in its border.
        let border = if lane || acting.is_some() {
            faded(accent, 0.4 + 0.6 * pulse(p))
        } else {
            faded(muted, 0.3)
        };
        let body = container(lines)
            .width(Length::Fill)
            .padding(8)
            .style(move |_: &Theme| {
                let (color, width) = if selected {
                    (accent, 2.0)
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
        let halo = container(body)
            .padding(3)
            .style(move |_: &Theme| container::Style {
                border: Border {
                    color: faded(accent, ring.unwrap_or(0.0)),
                    width: 2.0,
                    radius: 9.0.into(),
                },
                ..container::Style::default()
            });
        mouse_area(halo)
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press(Message::BoardSelect(column, row_at))
            .into()
    }

    /// What a master's pipeline claim stands for: `waits on dev #12` with a
    /// pulsing dot while it runs, `fixing dev #12` once it failed, else
    /// `holds dev #12`.
    fn wait_line<'a>(
        &'a self,
        client: &CiClient,
        claim: &ClaimView,
        p: f32,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let number = claim.subject.number();
        let status = client
            .channel_view(claim.subject.repo(), &claim.channel)
            .and_then(|view| client.views.get(&view))
            .and_then(|state| {
                state
                    .pipelines
                    .iter()
                    .find(|shown| shown.pipeline.number == number)
            })
            .map(|shown| shown.pipeline.status.clone());
        let what = format!("{} #{number}", claim.channel);
        match status.as_deref() {
            Some("running") => {
                let color = faded(self.theme.accent(), 0.3 + 0.7 * pulse(p));
                let dot = container(Space::new())
                    .width(6)
                    .height(6)
                    .style(move |_: &Theme| container::Style {
                        background: Some(color.into()),
                        border: iced::border::rounded(3.0),
                        ..container::Style::default()
                    });
                row![
                    dot,
                    text(format!("waits on {what}"))
                        .size(CAPTION_TEXT)
                        .color(muted)
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            }
            Some("failed") => text(format!("fixing {what}"))
                .size(CAPTION_TEXT)
                .color(self.theme.error())
                .into(),
            _ => text(format!("holds {what}"))
                .size(CAPTION_TEXT)
                .color(muted)
                .into(),
        }
    }

    /// Where a session's terminal runs and what its agent does: the host
    /// glyph, the role glyph and the state word, `running` without a
    /// report.
    fn running_line<'a>(
        &'a self,
        runner: RunnerKey,
        terminal: TerminalId,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut line = row![].spacing(4).align_y(Alignment::Center);
        if let Some((glyph, name)) = self.host_of(&runner) {
            line = line.push(super::tip(
                super::glyph_text(glyph, 12.0).color(muted),
                name,
            ));
        }
        match self.agent_at(&runner, terminal) {
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
        scroll::vertical(list).height(Length::Fill).into()
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

/// The Overview's row for a CI's Claude sign-in: while it is signed out,
/// ends within [`CLAUDE_WARN`], or could not renew. A press signs in
/// through this machine's browser.
fn claude_item(
    ci: &RunnerKey,
    claude: Option<&zeughaus_link::ClaudeView>,
    now: u64,
) -> Option<AttentionItem> {
    let claude = claude?;
    let (severity, title) = match &claude.account {
        None => (Severity::Error, "Claude: the CI is signed out".to_owned()),
        Some(_) if now + CLAUDE_WARN.as_secs() >= claude.ends => {
            let days = claude.ends.saturating_sub(now) / 86_400;
            (
                Severity::Warning,
                format!("Claude: the CI's sign-in ends in {days} days"),
            )
        }
        Some(_) if claude.error.is_some() => {
            (Severity::Warning, "Claude: the CI cannot renew".to_owned())
        }
        Some(_) => return None,
    };
    Some(AttentionItem {
        severity,
        glyph: glyph::WARNING,
        title,
        detail: claude
            .error
            .clone()
            .unwrap_or_else(|| "Enter signs in through the browser".to_owned()),
        since: (claude.signed_in > 0).then_some(claude.signed_in),
        target: AttentionTarget::ClaudeSignIn(ci.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::{ClaimHolder, DocKind};

    fn doc(number: u64, state: DocState, changed: u64) -> DocView {
        DocView {
            repo: "r".to_owned(),
            number,
            kind: DocKind::Ticket,
            title: format!("d{number}"),
            state,
            source: "r".to_owned(),
            created: 0,
            changed,
            approved: None,
            released: None,
            links: Vec::new(),
            merged: Vec::new(),
            into: None,
            runs_on: Vec::new(),
            placement: Vec::new(),
            body: String::new(),
            plan_text: String::new(),
            revision: 0,
            planner: None,
            auto_approve: false,
            feedback: String::new(),
            onboard: None,
            note: String::new(),
            waits_for: Vec::new(),
            filed: Vec::new(),
        }
    }

    fn session(id: u64, kind: SessionKind, doc: Option<u64>, state: SessionState) -> SessionView {
        SessionView {
            id,
            repo: "r".to_owned(),
            kind,
            doc,
            runner: "a".to_owned(),
            machines: Vec::new(),
            state,
            terminal: Some(3),
            started: 0,
            ended: None,
            code: None,
            message: String::new(),
        }
    }

    #[test]
    fn tickets_fall_into_the_lifecycle_slots_and_done_keeps_the_newest() {
        let mut blocked = doc(2, DocState::Ready, 1);
        blocked.waits_for = vec![zeughaus_link::DocLink {
            kind: zeughaus_link::LinkKind::DependsOn,
            number: 1,
            repo: None,
        }];
        let mut docs = vec![
            doc(1, DocState::Open, 1),
            blocked,
            doc(3, DocState::Ready, 1),
            doc(4, DocState::Review, 1),
            doc(5, DocState::Approved, 1),
            doc(6, DocState::InProgress, 1),
        ];
        docs.extend((10..30).map(|n| doc(n, DocState::Done, n)));
        let planning = session(7, SessionKind::Plan, None, SessionState::Running);
        let master = session(8, SessionKind::Master, Some(6), SessionState::Running);
        let slots = board_columns(&docs, &[&planning, &master], &[], false);
        // A ready ticket that waits for #1 stays with the proposals.
        assert_eq!(slots[PROPOSALS], [Card::Doc(2), Card::Doc(1)]);
        assert_eq!(slots[PLANNING], [Card::Doc(3), Card::Session(7)]);
        assert_eq!(slots[PLANS], [Card::Doc(4)]);
        assert_eq!(slots[QUEUE], [Card::Doc(5)]);
        assert!(slots[STARTING].is_empty());
        assert_eq!(slots[RUNNING], [Card::Doc(6)]);
        assert_eq!(slots[DONE].len(), DONE_SHOWN);
        assert_eq!(slots[DONE][0], Card::Doc(29));
    }

    #[test]
    fn work_in_passage_takes_the_lanes_and_a_merged_ticket_follows_its_plan() {
        let claim = ClaimView {
            subject: ClaimSubject::Ticket {
                repo: "r".to_owned(),
                number: 1,
            },
            holder: ClaimHolder {
                runner: "a".to_owned(),
                terminal: 3,
                session: None,
            },
            label: String::new(),
            channel: String::new(),
            since: 0,
            renewed: 0,
        };
        // Claimed, or planned by a live session: each in PLANNING.
        let docs = vec![
            doc(1, DocState::Open, 1),
            doc(2, DocState::Planning, 1),
            doc(4, DocState::Open, 1),
        ];
        let planning = session(7, SessionKind::Plan, Some(2), SessionState::Running);
        let slots = board_columns(&docs, &[&planning], &[&claim], false);
        assert_eq!(slots[PROPOSALS], [Card::Doc(4)]);
        assert_eq!(slots[PLANNING], [Card::Doc(2), Card::Doc(1)]);
        // A session asked for without a ticket has a card of its own.
        let slots = board_columns(&docs, &[], &[], true);
        assert_eq!(slots[PLANNING], [Card::Doc(2), Card::Opening]);

        // A merged ticket is left to the card of the ticket in review.
        let mut merged = doc(1, DocState::Merged, 2);
        merged.into = Some(5);
        let mut plan = doc(5, DocState::Review, 2);
        plan.merged = vec![1];
        let slots = board_columns(&[merged.clone(), plan.clone()], &[], &[], false);
        assert_eq!(slots[PLANS], [Card::Doc(5)]);
        assert!(slots.iter().all(|slot| !slot.contains(&Card::Doc(1))));
        // Done with its plan, it is done too.
        plan.state = DocState::Done;
        let slots = board_columns(&[merged, plan], &[], &[], false);
        assert_eq!(slots[DONE], [Card::Doc(5), Card::Doc(1)]);

        // A master that has not started yet puts its ticket in STARTING.
        let docs = vec![doc(6, DocState::InProgress, 3)];
        let starting = session(9, SessionKind::Master, Some(6), SessionState::Starting);
        let slots = board_columns(&docs, &[&starting], &[], false);
        assert_eq!(slots[STARTING], [Card::Doc(6)]);
        assert!(slots[RUNNING].is_empty());
    }
}
