//! A repository's Plans tab: the plans and proposals the CI stores for it,
//! approved, rejected and cleaned up here, the `Runs on` of each, the plan
//! sessions the CI opens on a runner that hosts them, and the master
//! sessions it dispatches approved plans to.
//!
//! The list is grouped the way an agent reads it (awaiting approval, the
//! queue, in progress, open proposals, history); the selected document is
//! drawn as Markdown beside it. The CI announces every change on `/events`
//! (`CiDocs`, `CiSessions`), and the tab asks for the list again.

use iced::widget::text::Wrapping;
use iced::widget::{
    Space, button, column, container, markdown, row, rule, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length, Task};
use zeughaus_link::{
    CiReply, CiRequest, DocKind, DocState, DocView, MachineView, SessionKind, SessionState,
    SessionView, doc_groups,
};
use zeughaus_mux::TerminalId;
use zeughaus_theme::Theme;

use super::{
    CiClient, Notice, SP_1, SP_2, SP_3, SP_4, TEXT_MD, TEXT_SM, TOOLBAR_HEIGHT, UNEXPECTED,
    action_button, ago, glyph, icon, now, tip,
};
use crate::app::App;
use crate::feed;
use crate::message::{CiMsg, CiShortcut, Message};
use crate::notices::Severity;
use crate::style::{self, Tokens};
use crate::transport::Endpoint;
use crate::workspace::{RunnerKey, TabRef};

/// The width of the document list.
const LIST_WIDTH: f32 = 360.0;
/// The widest a document's text grows.
const BODY_MAX_WIDTH: f32 = 900.0;

/// The glyphs of the Plans tab's buttons (codicons of the bundled Nerd Font).
const CHECK: char = '\u{eab2}';
const TRASH: char = '\u{ea81}';
const TERMINAL: char = '\u{ea85}';
const EDIT: char = '\u{ea73}';

/// One repository's Plans tab.
pub(super) struct PlansState {
    pub(super) repo: String,
    /// Bodies empty, in number order.
    docs: Vec<DocView>,
    loaded: bool,
    loading: bool,
    /// A change arrived while the list was on its way, which may not have
    /// seen it.
    again: bool,
    /// Why the list did not arrive.
    error: Option<String>,
    /// The document drawn beside the list.
    selected: Option<u64>,
    /// Its body, once the CI has sent it.
    shown: Option<(u64, markdown::Content)>,
    /// Why it did not.
    shown_error: Option<String>,
    /// An approval, rejection or deletion on its way: the buttons wait.
    acting: bool,
    /// What the last action or session request came to.
    notice: Option<Notice>,
    /// The `Runs on` being edited: the document and the text so far.
    pub(super) runs_on_input: Option<(u64, String)>,
}

impl PlansState {
    pub(super) fn new(repo: &str) -> PlansState {
        PlansState {
            repo: repo.to_owned(),
            docs: Vec::new(),
            loaded: false,
            loading: false,
            again: false,
            error: None,
            selected: None,
            shown: None,
            shown_error: None,
            acting: false,
            notice: None,
            runs_on_input: None,
        }
    }

    /// Whether the list has to be asked for: never loaded, or the last
    /// attempt failed.
    pub(super) fn behind(&self) -> bool {
        (!self.loaded || self.error.is_some()) && !self.loading
    }
}

impl CiClient {
    /// When the newest plan awaiting approval in Plans `view` last changed,
    /// once the list has loaded.
    pub(super) fn awaiting_since(&self, view: u64) -> Option<u64> {
        self.plans
            .get(&view)?
            .docs
            .iter()
            .filter(|doc| doc.kind == DocKind::Plan && doc.state == DocState::Saved)
            .map(|doc| doc.changed)
            .max()
    }

    /// Forgets that a list was on its way: the connection that carried it
    /// ended.
    pub(super) fn restart_docs(&mut self, view: u64) {
        if let Some(state) = self.plans.get_mut(&view) {
            state.loading = false;
            state.again = false;
        }
    }

    /// The list of a Plans tab; one at a time, a second ask while one is on
    /// its way is answered by asking again once it is in.
    pub(super) fn ask_docs(&mut self, key: &RunnerKey, view: u64) -> Task<Message> {
        let Some(state) = self.plans.get_mut(&view) else {
            return Task::none();
        };
        if state.loading {
            state.again = true;
            return Task::none();
        }
        state.loading = true;
        let (key, repo) = (key.clone(), state.repo.clone());
        Task::perform(docs(self.endpoint.clone(), repo), move |result| {
            Message::Ci(key, CiMsg::Docs { view, result })
        })
    }

    pub(super) fn docs_arrived(
        &mut self,
        key: &RunnerKey,
        view: u64,
        result: Result<Vec<DocView>, String>,
    ) -> Task<Message> {
        let Some(state) = self.plans.get_mut(&view) else {
            return Task::none();
        };
        state.loading = false;
        match result {
            Ok(docs) => {
                state.docs = docs;
                state.loaded = true;
                state.error = None;
                // A deleted document leaves nothing to show.
                if let Some(selected) = state.selected
                    && !state.docs.iter().any(|doc| doc.number == selected)
                {
                    state.selected = None;
                    state.shown = None;
                    state.shown_error = None;
                }
            }
            Err(e) => state.error = Some(e),
        }
        if std::mem::take(&mut state.again) {
            self.ask_docs(key, view)
        } else {
            Task::none()
        }
    }

    /// Shows a document beside the list, its body asked for now.
    pub(super) fn select(&mut self, key: &RunnerKey, view: u64, number: u64) -> Task<Message> {
        let Some(state) = self.plans.get_mut(&view) else {
            return Task::none();
        };
        if state.selected == Some(number) && state.shown_error.is_none() {
            return Task::none();
        }
        state.selected = Some(number);
        state.shown = None;
        state.shown_error = None;
        let (key, repo) = (key.clone(), state.repo.clone());
        Task::perform(doc(self.endpoint.clone(), repo, number), move |result| {
            Message::Ci(
                key,
                CiMsg::Shown {
                    view,
                    number,
                    result,
                },
            )
        })
    }

    pub(super) fn shown_arrived(
        &mut self,
        view: u64,
        number: u64,
        result: Result<Option<Box<DocView>>, String>,
    ) {
        let Some(state) = self.plans.get_mut(&view) else {
            return;
        };
        // The user has picked another one meanwhile.
        if state.selected != Some(number) {
            return;
        }
        match result {
            Ok(Some(doc)) => {
                state.shown = Some((number, markdown::Content::parse(&doc.shown_text())))
            }
            Ok(None) => state.shown_error = Some(format!("#{number} is gone")),
            Err(e) => state.shown_error = Some(e),
        }
    }

    /// Approves, rejects, requeues, finishes or deletes a document.
    pub(super) fn doc_act(
        &mut self,
        key: &RunnerKey,
        view: u64,
        request: CiRequest,
    ) -> Task<Message> {
        let Some(state) = self.plans.get_mut(&view) else {
            return Task::none();
        };
        if state.acting {
            return Task::none();
        }
        state.acting = true;
        state.notice = None;
        let key = key.clone();
        Task::perform(act(self.endpoint.clone(), request), move |result| {
            Message::Ci(key, CiMsg::DocActed { view, result })
        })
    }

    pub(super) fn doc_acted(
        &mut self,
        key: &RunnerKey,
        view: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        let Some(state) = self.plans.get_mut(&view) else {
            return Task::none();
        };
        state.acting = false;
        if let Err(e) = result {
            state.notice = Some(Notice {
                text: e,
                failed: true,
            });
        }
        // The counts on the tab come with the overview.
        let overview = if self.overview_pending {
            self.overview_again = true;
            Task::none()
        } else {
            self.ask_overview(key)
        };
        Task::batch([self.ask_docs(key, view), overview])
    }
}

async fn docs(endpoint: Endpoint, repo: String) -> Result<Vec<DocView>, String> {
    match feed::ci(endpoint, CiRequest::Docs { repo }).await? {
        CiReply::Docs { docs } => Ok(docs),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn doc(
    endpoint: Endpoint,
    repo: String,
    number: u64,
) -> Result<Option<Box<DocView>>, String> {
    match feed::ci(endpoint, CiRequest::Doc { repo, number }).await? {
        CiReply::Doc { doc } => Ok(doc),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn act(endpoint: Endpoint, request: CiRequest) -> Result<(), String> {
    match feed::ci(endpoint, request).await? {
        CiReply::Accepted => Ok(()),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn open(endpoint: Endpoint, request: CiRequest) -> Result<SessionView, String> {
    match feed::ci(endpoint, request).await? {
        CiReply::Session { session } => Ok(session),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

/// What a document can be asked to do from its state: the button's glyph,
/// words, tooltip and the state it moves to (`None`: deleted).
fn actions(doc: &DocView) -> Vec<(char, &'static str, &'static str, Option<DocState>)> {
    match (doc.kind, doc.state) {
        (DocKind::Plan, DocState::Saved) => vec![
            (
                CHECK,
                "Approve",
                "Queue it for a master session on a runner it fits",
                Some(DocState::Approved),
            ),
            (
                glyph::CLOSE,
                "Reject",
                "It will not be executed",
                Some(DocState::Rejected),
            ),
        ],
        (DocKind::Plan, DocState::Approved) => vec![(
            glyph::CLOSE,
            "Reject",
            "Take it out of the queue",
            Some(DocState::Rejected),
        )],
        (DocKind::Plan, DocState::InProgress) => vec![
            (
                CHECK,
                "Mark done",
                "Its work is finished",
                Some(DocState::Done),
            ),
            (
                glyph::RERUN,
                "Requeue",
                "Back into the queue, at its place",
                Some(DocState::Approved),
            ),
        ],
        (DocKind::Proposal, DocState::Open) => vec![(
            glyph::CLOSE,
            "Dismiss",
            "No plan will take it up",
            Some(DocState::Dismissed),
        )],
        (DocKind::Onboarding, DocState::Open) => vec![
            (
                CHECK,
                "Approve",
                "The CI adds the bot and the hook, stores the webhook secret, registers the \
                 repository and starts its first pipeline",
                Some(DocState::Approved),
            ),
            (
                glyph::CLOSE,
                "Reject",
                "It will not join the CI",
                Some(DocState::Rejected),
            ),
        ],
        // The CI is working on it.
        (DocKind::Onboarding, DocState::Approved) => Vec::new(),
        _ => vec![(TRASH, "Delete", "Remove it from the CI", None)],
    }
}

impl CiClient {
    /// The runner `name` as the overview lists it.
    pub(super) fn runner_view(&self, name: &str) -> Option<&MachineView> {
        self.overview
            .as_ref()?
            .machines
            .iter()
            .find(|m| m.kind == "runner" && m.name == name)
    }

    /// Whether a connected runner hosts sessions.
    fn sessions_possible(&self) -> bool {
        self.overview.as_ref().is_some_and(|overview| {
            overview
                .machines
                .iter()
                .any(|m| m.kind == "runner" && m.sessions && m.status != "offline")
        })
    }

    /// The live master session of plan `number` of `repo`.
    fn master_of(&self, repo: &str, number: u64) -> Option<&SessionView> {
        self.overview.as_ref()?.sessions.iter().find(|s| {
            s.kind == SessionKind::Master
                && s.repo == repo
                && s.doc == Some(number)
                && s.state.is_live()
        })
    }

    /// The runner key of runner `name`: its pinned fingerprint.
    pub(super) fn runner_key(&self, name: &str) -> Option<RunnerKey> {
        let fingerprint = self
            .runner_view(name)?
            .fingerprint
            .parse::<weida::Fingerprint>()
            .ok()?;
        Some(RunnerKey::new(&fingerprint.to_string()))
    }
}

/// The `Runs on` a text field says: `;`-separated selectors, trimmed; an
/// empty text is any runner.
fn runs_on_of(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.split(';').map(|part| part.trim().to_owned()).collect()
}

impl App {
    /// A plan or proposal of `repo` changed: its Plans tab asks for the list
    /// again, and the overview for the counts.
    pub(in crate::app) fn apply_ci_docs_event(
        &mut self,
        key: &RunnerKey,
        repo: String,
    ) -> Task<Message> {
        let Some(client) = self.ci.get_mut(key) else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        if let Some(view) = client.repos.get(&repo).map(|known| known.plans) {
            tasks.push(client.ask_docs(key, view));
        }
        if client.overview_pending {
            client.overview_again = true;
        } else {
            tasks.push(client.ask_overview(key));
        }
        Task::batch(tasks)
    }

    /// A session of the CI changed: the overview, which lists them, is asked
    /// for again.
    pub(in crate::app) fn apply_ci_sessions_event(&mut self, key: &RunnerKey) -> Task<Message> {
        let Some(client) = self.ci.get_mut(key) else {
            return Task::none();
        };
        if client.overview_pending {
            client.overview_again = true;
            Task::none()
        } else {
            client.ask_overview(key)
        }
    }

    /// The name the CI knows this editor's own runner by, when that runner
    /// hosts sessions: the first of the runner URLs is its endpoint file.
    fn own_session_runner(&self, client: &CiClient) -> Option<String> {
        let url = self.runner_urls.first()?;
        let own = weida::EndpointAddr::parse(url).ok()?.peer?.to_string();
        client
            .overview
            .as_ref()?
            .machines
            .iter()
            .find(|m| {
                m.kind == "runner"
                    && m.sessions
                    && m.fingerprint
                        .parse::<weida::Fingerprint>()
                        .is_ok_and(|fp| fp.to_string() == own)
            })
            .map(|m| m.name.clone())
    }

    /// Asks the CI for a plan session of the Plans tab's repository, on this
    /// editor's own runner when it hosts sessions; with `proposal` it plans
    /// that open proposal.
    pub(super) fn open_session(
        &mut self,
        key: &RunnerKey,
        view: u64,
        proposal: Option<u64>,
    ) -> Task<Message> {
        let Some(client) = self.ci.get(key) else {
            return Task::none();
        };
        let runner = self.own_session_runner(client);
        let Some(state) = client.plans.get(&view) else {
            return Task::none();
        };
        let request = CiRequest::OpenSession {
            repo: state.repo.clone(),
            proposal,
            runner,
        };
        let endpoint = client.endpoint.clone();
        if let Some(state) = self.ci.get_mut(key).and_then(|c| c.plans.get_mut(&view)) {
            state.notice = None;
        }
        let key = key.clone();
        Task::perform(open(endpoint, request), move |result| {
            Message::Ci(key, CiMsg::SessionOpened(result))
        })
    }

    /// The CI took the session: its tab comes to the front once its runner
    /// runs it. A refusal is a notice.
    pub(super) fn session_opened(
        &mut self,
        key: &RunnerKey,
        result: Result<SessionView, String>,
    ) -> Task<Message> {
        match result {
            Ok(session) => {
                self.reveal_session = Some((key.clone(), session.id));
                self.apply_ci_sessions_event(key)
            }
            Err(e) => {
                self.raise(None, Severity::Error, "Session failed", e, false, None);
                Task::none()
            }
        }
    }

    /// Brings the tab of a terminal a runner just opened for this editor to
    /// the front, once that runner's workspace has it. A session the CI
    /// opened names its terminal once its runner runs it; one that failed
    /// says why.
    pub(in crate::app) fn reveal_opened_terminal(&mut self) {
        if let Some((ci, id)) = self.reveal_session.clone() {
            let session = self.ci.get(&ci).and_then(|client| {
                let session = client
                    .overview
                    .as_ref()?
                    .sessions
                    .iter()
                    .find(|s| s.id == id)?;
                Some((session.clone(), client.runner_key(&session.runner)))
            });
            match session {
                Some((session, _)) if session.state == SessionState::Failed => {
                    self.reveal_session = None;
                    let message = format!("session {id}: {}", session.message);
                    self.raise(
                        None,
                        Severity::Error,
                        "Session failed",
                        message,
                        false,
                        None,
                    );
                }
                Some((session, Some(runner))) if session.terminal.is_some() => {
                    self.reveal_session = None;
                    self.reveal = session.terminal.map(|t| (runner, TerminalId(t)));
                }
                _ => {}
            }
        }
        let Some((runner, terminal)) = self.reveal.clone() else {
            return;
        };
        let tab = self.workspace.section(&runner).and_then(|section| {
            section
                .snapshot
                .tabs()
                .find(|tab| tab.root.terminals().any(|shown| shown == terminal))
                .map(|tab| TabRef {
                    runner: runner.clone(),
                    tab: tab.id,
                })
        });
        if let Some(tab) = tab {
            self.reveal = None;
            if self.workspace.active_tab() != Some(&tab) {
                self.workspace.activate(tab);
            }
        }
    }

    /// A link in a document, opened by the platform's handler.
    pub(super) fn open_doc_link(&mut self, url: &str) -> Task<Message> {
        if let Err(e) = super::super::terminal::open_link(url) {
            self.hint = Some((format!("{url}: {e}"), iced::time::Instant::now()));
        }
        Task::none()
    }

    /// A repository's Plans tab: its toolbar, the list of its documents and
    /// the selected one beside it.
    pub(super) fn plans_view<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        view: u64,
        state: &'a PlansState,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut toolbar = row![
            text(state.repo.as_str()).size(TEXT_MD).color(muted),
            icon(glyph::CHEVRON_RIGHT, TEXT_MD, muted),
            text("Plans").size(TEXT_MD),
            Space::new().width(Length::Fill),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center)
        .padding([SP_2, SP_4])
        .height(TOOLBAR_HEIGHT);
        if let Some(stale) = self.stale(client) {
            toolbar = toolbar.push(stale);
        }
        if let Some(notice) = &state.notice {
            let color = if notice.failed {
                self.theme.error()
            } else {
                muted
            };
            toolbar = toolbar.push(text(notice.text.as_str()).size(TEXT_SM).color(color));
        }
        if state.loaded
            && let Some(error) = &state.error
        {
            toolbar = toolbar.push(text(error.as_str()).size(TEXT_SM).color(self.theme.error()));
        }
        let possible = client.sessions_possible();
        let mut pressed = action_button(EDIT, "Plan session");
        if possible {
            pressed = pressed.on_press_with(move || {
                Message::Ci(
                    runner.clone(),
                    CiMsg::OpenSession {
                        view,
                        proposal: None,
                    },
                )
            });
        }
        let what = if possible {
            "omp in plan mode on a session runner, through the CI"
        } else {
            "no connected runner hosts sessions (sessions = true in ci.toml)"
        };
        toolbar = toolbar.push(tip(pressed, what.to_owned()));
        let paused = client
            .overview
            .as_ref()
            .and_then(|o| o.repos.iter().find(|r| r.repo == state.repo))
            .is_some_and(|r| r.paused);
        let (glyph, words, what) = if paused {
            (
                TERMINAL,
                "Resume queue",
                "Dispatch the approved plans to runners again",
            )
        } else {
            (
                TERMINAL,
                "Pause queue",
                "Start no master for the approved plans until resumed",
            )
        };
        let mut pressed = action_button(glyph, words);
        if !state.acting {
            let request = CiRequest::QueuePaused {
                repo: state.repo.clone(),
                paused: !paused,
            };
            pressed = pressed.on_press_with(move || {
                Message::Ci(
                    runner.clone(),
                    CiMsg::DocAct {
                        view,
                        request: request.clone(),
                    },
                )
            });
        }
        toolbar = toolbar.push(tip(pressed, what.to_owned()));

        let body: Element<'a, Message, Theme> = if state.docs.is_empty() {
            let why = match (&state.error, state.loaded) {
                (Some(error), _) => error.as_str(),
                (None, true) => {
                    "No plans or proposals yet. A plan session saves its plan from the plan \
                     review; another project's session writes a proposal with proposal_write."
                }
                (None, false) => "Loading plans.",
            };
            super::super::unavailable("Plans", why)
        } else {
            row![
                self.doc_list(runner, client, view, state),
                rule::vertical(1),
                self.doc_detail(runner, client, view, state),
            ]
            .height(Length::Fill)
            .into()
        };
        column![toolbar, rule::horizontal(1), body].into()
    }

    /// The documents in their groups, one dense row each that selects it:
    /// its number and title, its `Runs on` selectors, where an in-progress
    /// plan runs, and the age of its last change.
    fn doc_list<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        view: u64,
        state: &'a PlansState,
    ) -> Element<'a, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let muted = self.theme.muted();
        let now = now();
        let mut list = column![].spacing(SP_1).padding([SP_3, SP_2]);
        for (group, docs) in doc_groups(&state.docs) {
            list = list.push(
                container(
                    text(group.title().to_uppercase())
                        .size(TEXT_SM)
                        .color(muted),
                )
                .padding([SP_2, SP_2]),
            );
            for doc in docs {
                let number = doc.number;
                let selected = state.selected == Some(number);
                let mut line = row![
                    text(format!("#{number}")).size(TEXT_SM).color(muted),
                    text(doc.title.as_str())
                        .size(13)
                        .wrapping(Wrapping::None)
                        .width(Length::Fill),
                ]
                .spacing(SP_2)
                .align_y(Alignment::Center);
                for selector in &doc.runs_on {
                    line = line.push(
                        container(text(selector.as_str()).size(11))
                            .padding([0, 6])
                            .style(style::badge(&t, false)),
                    );
                }
                if doc.state == DocState::InProgress
                    && let Some(session) = client.master_of(&doc.repo, number)
                {
                    line = line.push(text(session.runner.as_str()).size(TEXT_SM).color(muted));
                }
                line = line.push(text(ago(doc.changed, now)).size(TEXT_SM).color(muted));
                list = list.push(
                    container(
                        button(container(line).clip(true))
                            .width(Length::Fill)
                            .padding([SP_1 + 1.0, SP_2])
                            .style(style::row_entry_button(&t, selected))
                            .on_press_with(move || {
                                Message::Ci(runner.clone(), CiMsg::Select { view, number })
                            }),
                    )
                    .style(style::row_highlight(&t, selected)),
                );
            }
        }
        scrollable(list)
            .width(LIST_WIDTH)
            .height(Length::Fill)
            .into()
    }

    /// The selected document: an action bar that stays on top, the master's
    /// terminal under it while one works on the plan, then the title, what
    /// the document is and its text.
    fn doc_detail<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        view: u64,
        state: &'a PlansState,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let Some(doc) = state
            .selected
            .and_then(|number| state.docs.iter().find(|doc| doc.number == number))
        else {
            return container(
                text("Select a plan or proposal (j, k or the arrows).")
                    .size(TEXT_SM)
                    .color(muted),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into();
        };
        let now = now();
        let mut meta = format!(
            "{} \u{b7} {} \u{b7} from {} \u{b7} created {}",
            doc.kind.name(),
            doc.state.name().replace('_', " "),
            doc.source,
            ago(doc.created, now)
        );
        if let Some(plan) = doc.plan {
            meta.push_str(&format!(" \u{b7} taken up by #{plan}"));
        }
        if !doc.proposals.is_empty() {
            let named: Vec<String> = doc.proposals.iter().map(|n| format!("#{n}")).collect();
            meta.push_str(&format!(" \u{b7} from proposals {}", named.join(", ")));
        }
        if let Some(place) = master_place(client, doc) {
            meta.push_str(&format!(" \u{b7} master {place}"));
        }
        let text_part: Element<'a, Message, Theme> = match (&state.shown, &state.shown_error) {
            (Some((number, content)), _) if *number == doc.number => {
                let settings = markdown::Settings::with_text_size(
                    TEXT_MD,
                    markdown::Style::from_palette(self.theme.base().palette()),
                );
                markdown::view(content.items(), settings)
                    .map(move |uri| Message::Ci(runner.clone(), CiMsg::OpenLink(uri)))
            }
            (_, Some(error)) => text(error.as_str())
                .size(TEXT_SM)
                .color(self.theme.error())
                .into(),
            _ => text("Loading.").size(TEXT_SM).color(muted).into(),
        };
        // The body opens with the title as its heading.
        let mut head = column![text(meta).size(TEXT_SM).color(muted)].spacing(SP_2);
        if let Some(line) = self.runs_on_line(runner, view, state, doc) {
            head = head.push(line);
        }
        let body = scrollable(
            column![head, rule::horizontal(1), text_part]
                .spacing(SP_3)
                .padding(SP_4)
                .max_width(BODY_MAX_WIDTH),
        )
        .width(Length::Fill)
        .height(Length::FillPortion(1));
        let mut detail = column![
            self.action_bar(runner, client, view, state, doc),
            rule::horizontal(1),
        ];
        if let Some((key, terminal)) = self.docked_master(runner, view) {
            detail = detail
                .push(
                    container(self.page_terminal(&key, terminal))
                        .width(Length::Fill)
                        .height(Length::FillPortion(1)),
                )
                .push(rule::horizontal(1));
        }
        detail.push(body).height(Length::Fill).into()
    }

    /// The buttons a document's state allows, the primary one filled:
    /// `Approve` for a saved plan, `Plan this proposal` for an open
    /// proposal, `Open` for a plan a master works on, with `Stop` beside it.
    fn action_bar<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        view: u64,
        state: &'a PlansState,
        doc: &'a DocView,
    ) -> Element<'a, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let mut bar = row![].spacing(SP_2).align_y(Alignment::Center);
        let mut primary_taken = false;
        let pill = |words: &'a str, primary: bool| {
            let (fill, quiet) = (style::pill_button(&t), style::secondary_button(&t));
            button(text(words).size(TEXT_SM))
                .padding([SP_1, SP_3])
                .style(move |theme: &Theme, status| {
                    if primary {
                        fill(theme, status)
                    } else {
                        quiet(theme, status)
                    }
                })
        };
        if doc.kind == DocKind::Proposal && doc.state == DocState::Open {
            let number = doc.number;
            let mut pressed = pill("Plan this proposal", true);
            let what = if client.sessions_possible() {
                pressed = pressed.on_press_with(move || {
                    Message::Ci(
                        runner.clone(),
                        CiMsg::OpenSession {
                            view,
                            proposal: Some(number),
                        },
                    )
                });
                "omp plans it in plan mode on a session runner (p)"
            } else {
                "no connected runner hosts sessions (sessions = true in ci.toml)"
            };
            bar = bar.push(tip(pressed, what.to_owned()));
            primary_taken = true;
        }
        if let Some(session) = client.master_of(&doc.repo, doc.number) {
            let mut open = pill("Open", true);
            if let (Some(key), Some(terminal)) =
                (client.runner_key(&session.runner), session.terminal)
            {
                open = open.on_press_with(move || {
                    Message::RevealTerminal(key.clone(), TerminalId(terminal))
                });
            }
            bar = bar.push(tip(open, "Bring the master's tab to the front".to_owned()));
            let mut stop = button(text("Stop").size(TEXT_SM))
                .padding([SP_1, SP_3])
                .style(style::danger_button(&t));
            if !state.acting {
                let request = CiRequest::CloseSession { id: session.id };
                stop = stop.on_press_with(move || {
                    Message::Ci(
                        runner.clone(),
                        CiMsg::DocAct {
                            view,
                            request: request.clone(),
                        },
                    )
                });
            }
            bar = bar.push(tip(stop, "End the master's session".to_owned()));
            primary_taken = true;
        }
        for (_, words, what, to) in actions(doc) {
            let primary = !primary_taken && words == "Approve";
            primary_taken |= primary;
            let request = match to {
                Some(state) => CiRequest::DocState {
                    repo: doc.repo.clone(),
                    number: doc.number,
                    state,
                },
                None => CiRequest::DocDelete {
                    repo: doc.repo.clone(),
                    number: doc.number,
                },
            };
            let mut pressed = pill(words, primary);
            if !state.acting {
                pressed = pressed.on_press_with(move || {
                    Message::Ci(
                        runner.clone(),
                        CiMsg::DocAct {
                            view,
                            request: request.clone(),
                        },
                    )
                });
            }
            let key = match words {
                "Approve" => " (a)",
                "Reject" => " (r)",
                _ => "",
            };
            bar = bar.push(tip(pressed, format!("{what}{key}")));
        }
        container(bar).padding([SP_2, SP_4]).into()
    }

    /// The `Runs on` of a document: a field while it can change (an open
    /// proposal, a saved or approved plan), else a line when it names any.
    fn runs_on_line<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a PlansState,
        doc: &'a DocView,
    ) -> Option<Element<'a, Message, Theme>> {
        let muted = self.theme.muted();
        let editable = matches!(
            (doc.kind, doc.state),
            (DocKind::Proposal, DocState::Open)
                | (DocKind::Plan, DocState::Saved)
                | (DocKind::Plan, DocState::Approved)
        );
        let label = text("Runs on").size(TEXT_SM).color(muted);
        if !editable {
            if doc.runs_on.is_empty() {
                return None;
            }
            return Some(
                row![label, text(doc.runs_on.join("; ")).size(TEXT_SM)]
                    .spacing(SP_2)
                    .into(),
            );
        }
        let number = doc.number;
        let value = match &state.runs_on_input {
            Some((edited, value)) if *edited == number => value.clone(),
            _ => doc.runs_on.join("; "),
        };
        let request = CiRequest::DocRunsOn {
            repo: doc.repo.clone(),
            number,
            runs_on: runs_on_of(&value),
        };
        let field = text_input("any runner, or os=windows; os=linux", &value)
            .size(TEXT_SM)
            .on_input(move |text| {
                Message::Ci(runner.clone(), CiMsg::RunsOnEdited { view, number, text })
            })
            .on_submit(Message::Ci(runner.clone(), CiMsg::DocAct { view, request }));
        Some(
            row![label, field]
                .spacing(SP_2)
                .align_y(Alignment::Center)
                .into(),
        )
    }

    /// The master's runner and terminal of the plan selected in Plans `view`
    /// of CI `ci`, while one works on it: what the plan's detail docks.
    pub(super) fn docked_master(
        &self,
        ci: &RunnerKey,
        view: u64,
    ) -> Option<(RunnerKey, TerminalId)> {
        let client = self.ci.get(ci)?;
        let state = client.plans.get(&view)?;
        let number = state.selected?;
        let session = client.master_of(&state.repo, number)?;
        Some((
            client.runner_key(&session.runner)?,
            TerminalId(session.terminal?),
        ))
    }

    /// Moves the selection of Plans `view` `step` documents down the list
    /// (up when negative), in the order the list shows them.
    pub(super) fn plans_step(&mut self, ci: &RunnerKey, view: u64, step: i32) -> Task<Message> {
        let Some(client) = self.ci.get_mut(ci) else {
            return Task::none();
        };
        let Some(state) = client.plans.get(&view) else {
            return Task::none();
        };
        let order: Vec<u64> = doc_groups(&state.docs)
            .into_iter()
            .flat_map(|(_, docs)| docs.into_iter().map(|doc| doc.number))
            .collect();
        if order.is_empty() {
            return Task::none();
        }
        let at = match state
            .selected
            .and_then(|n| order.iter().position(|&o| o == n))
        {
            Some(at) => (at as i64 + i64::from(step)).clamp(0, order.len() as i64 - 1) as usize,
            None => 0,
        };
        client.select(ci, view, order[at])
    }

    /// `a`, `r` and `p` on the selected document of Plans `view`: approve,
    /// reject, or plan an open proposal, each only where the document
    /// offers it.
    pub(super) fn plans_letter(
        &mut self,
        ci: &RunnerKey,
        view: u64,
        shortcut: CiShortcut,
    ) -> Task<Message> {
        let Some(client) = self.ci.get(ci) else {
            return Task::none();
        };
        let Some(doc) = client.plans.get(&view).and_then(|state| {
            let number = state.selected?;
            state.docs.iter().find(|doc| doc.number == number)
        }) else {
            return Task::none();
        };
        let wanted = match shortcut {
            CiShortcut::Approve => "Approve",
            CiShortcut::Rerun => "Reject",
            CiShortcut::Plan => {
                if doc.kind == DocKind::Proposal
                    && doc.state == DocState::Open
                    && client.sessions_possible()
                {
                    let proposal = Some(doc.number);
                    return self.open_session(ci, view, proposal);
                }
                return Task::none();
            }
            _ => return Task::none(),
        };
        let Some((_, _, _, to)) = actions(doc).into_iter().find(|(_, w, _, _)| *w == wanted) else {
            return Task::none();
        };
        let request = match to {
            Some(state) => CiRequest::DocState {
                repo: doc.repo.clone(),
                number: doc.number,
                state,
            },
            None => CiRequest::DocDelete {
                repo: doc.repo.clone(),
                number: doc.number,
            },
        };
        let key = ci.clone();
        self.apply_ci(key, CiMsg::DocAct { view, request })
    }
}

/// Where a plan's live master runs: `on <runner> (+ <machines>)`.
fn master_place(client: &CiClient, doc: &DocView) -> Option<String> {
    let session = client.master_of(&doc.repo, doc.number)?;
    let mut place = format!("on {}", session.runner);
    if !session.machines.is_empty() {
        let names: Vec<&str> = session.machines.iter().map(|m| m.name.as_str()).collect();
        place.push_str(&format!(" (+ {})", names.join(", ")));
    }
    Some(place)
}

#[cfg(test)]
mod tests {
    use zeughaus_link::{ChannelView, CiOverview, RepoDocsView};

    use super::*;

    fn overview(channels: &[(&str, &str)], repos: &[&str]) -> CiOverview {
        CiOverview {
            channels: channels
                .iter()
                .map(|&(repo, channel)| ChannelView {
                    repo: repo.to_owned(),
                    channel: channel.to_owned(),
                    latest: 1,
                    status: "succeeded".to_owned(),
                    load: None,
                })
                .collect(),
            machines: Vec::new(),
            queued: Vec::new(),
            platforms: Vec::new(),
            repos: repos
                .iter()
                .map(|repo| RepoDocsView {
                    repo: (*repo).to_owned(),
                    origin: format!("git.example.org/o/{repo}"),
                    default_branch: "main".to_owned(),
                    awaiting: 0,
                    queued: 0,
                    in_progress: Vec::new(),
                    paused: false,
                    proposals: 0,
                    onboarding: None,
                })
                .collect(),
            build: 0,
            sessions: Vec::new(),
        }
    }

    fn client() -> CiClient {
        CiClient::new(Endpoint("weida://ci/".to_owned()), "ci".to_owned())
    }

    /// Each repository with its channels, by name.
    fn repos(client: &CiClient) -> Vec<(String, Vec<String>)> {
        client
            .repos
            .iter()
            .map(|(name, repo)| (name.clone(), repo.channels.keys().cloned().collect()))
            .collect()
    }

    /// A repository of `ci.toml` without a pipeline yet still gets its Plans
    /// view, so its plans can be approved before anything ran.
    #[test]
    fn a_repository_without_channels_still_gets_its_plans() {
        let key = RunnerKey::new("sha256:ci");
        let mut client = client();
        let _ = client.overview_arrived(
            &key,
            Ok(overview(&[("griasdi", "main")], &["demo", "griasdi"])),
        );
        assert_eq!(
            repos(&client),
            vec![
                ("demo".to_owned(), vec![]),
                ("griasdi".to_owned(), vec!["main".to_owned()]),
            ]
        );
        let plans = client.repos["demo"].plans;
        assert_eq!(client.plans[&plans].repo, "demo");
    }

    #[test]
    fn a_repository_dropped_from_both_lists_loses_its_group_and_plans_tab() {
        let key = RunnerKey::new("sha256:ci");
        let mut client = client();
        let _ = client.overview_arrived(&key, Ok(overview(&[("griasdi", "main")], &["demo"])));
        let kept = client.repos["griasdi"].plans;
        let _ = client.overview_arrived(&key, Ok(overview(&[("griasdi", "main")], &[])));
        assert!(!client.repos.contains_key("demo"));
        assert_eq!(client.plans.keys().copied().collect::<Vec<_>>(), vec![kept]);
        // Listed only in `repos` now: still there, the same tab.
        let _ = client.overview_arrived(&key, Ok(overview(&[], &["griasdi"])));
        assert_eq!(client.repos["griasdi"].plans, kept);
        assert!(client.views.is_empty());
    }
}
