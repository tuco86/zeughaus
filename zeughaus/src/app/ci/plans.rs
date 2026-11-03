//! A repository's tickets as the Board and the ticket popout show them: the
//! list the CI stores, the ticket the popout shows with its proposal and
//! its plan in sections, what each state lets the user do, the `Runs on`
//! of each, the plan sessions the CI opens on a runner that hosts them and
//! the master sessions it dispatches approved tickets to.
//!
//! The CI announces every change on `/events` (`CiDocs`, `CiSessions`), and
//! the list is asked for again; a shown ticket whose list entry changed is
//! asked for again too.

use iced::widget::{Space, button, markdown, row, text, text_input};
use iced::{Alignment, Element, Length, Task};
use zeughaus_link::{
    CiReply, CiRequest, ClaimSubject, DocKind, DocState, DocView, MachineView, SessionKind,
    SessionState, SessionView, blocked_by,
};
use zeughaus_mux::TerminalId;
use zeughaus_theme::Theme;

use super::{
    CiClient, Notice, SP_1, SP_2, SP_3, SP_4, TEXT_MD, TEXT_SM, TOOLBAR_HEIGHT, UNEXPECTED,
    action_button, glyph, icon, tip,
};
use crate::app::App;
use crate::feed;
use crate::message::{CiMsg, CiShortcut, Message, TicketMsg};
use crate::notices::Severity;
use crate::style::{self, Tokens};
use crate::transport::Endpoint;
use crate::workspace::{RunnerKey, TabRef};

/// The widest a ticket's text grows.
pub(super) const BODY_MAX_WIDTH: f32 = 900.0;

/// The glyphs of the ticket buttons (codicons of the bundled Nerd Font).
pub(super) const CHECK: char = '\u{eab2}';
pub(super) const TRASH: char = '\u{ea81}';
pub(super) const EDIT: char = '\u{ea73}';
const PAUSE: char = '\u{ead1}';
const PLAY: char = '\u{eb2c}';

/// The ticket the popout shows, with its texts parsed.
pub(super) struct Shown {
    pub(super) number: u64,
    pub(super) doc: DocView,
    pub(super) body: markdown::Content,
    /// The plan in its sections, each with its own Markdown.
    pub(super) sections: Vec<(super::review::Section, markdown::Content)>,
}

/// One repository's tickets.
pub(super) struct PlansState {
    pub(super) repo: String,
    /// Texts empty, in number order.
    docs: Vec<DocView>,
    loaded: bool,
    loading: bool,
    /// A change arrived while the list was on its way, which may not have
    /// seen it.
    again: bool,
    /// Why the list did not arrive.
    error: Option<String>,
    /// The ticket the popout shows.
    selected: Option<u64>,
    /// It with its texts, once the CI has sent it.
    pub(super) shown: Option<Shown>,
    /// Why it did not.
    pub(super) shown_error: Option<String>,
    /// An action on its way: the buttons wait.
    pub(super) acting: bool,
    /// The document an action is on and what it does (`approving`), from
    /// the request until the list shows its result: its card pulses.
    pub(super) acting_on: Option<(u64, &'static str)>,
    /// What the last action or session request came to.
    pub(super) notice: Option<Notice>,
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
            acting_on: None,
            notice: None,
            runs_on_input: None,
        }
    }

    /// Whether the list has to be asked for: never loaded, or the last
    /// attempt failed.
    pub(super) fn behind(&self) -> bool {
        (!self.loaded || self.error.is_some()) && !self.loading
    }

    /// The documents, bodies empty, in number order.
    pub(super) fn docs(&self) -> &[DocView] {
        &self.docs
    }

    /// Ticket `number` as the list has it.
    pub(super) fn doc(&self, number: u64) -> Option<&DocView> {
        self.docs.iter().find(|doc| doc.number == number)
    }
}

impl CiClient {
    /// When the newest ticket awaiting review in Plans `view` last changed,
    /// once the list has loaded.
    pub(super) fn awaiting_since(&self, view: u64) -> Option<u64> {
        self.plans
            .get(&view)?
            .docs
            .iter()
            .filter(|doc| doc.kind == DocKind::Ticket && doc.state == DocState::Review)
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
                if !state.acting {
                    state.acting_on = None;
                }
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
        // The shown ticket changed: its texts are asked for again.
        let stale = state.shown.as_ref().and_then(|shown| {
            let listed = state.docs.iter().find(|doc| doc.number == shown.number)?;
            (listed.changed != shown.doc.changed).then_some(shown.number)
        });
        let again = std::mem::take(&mut state.again);
        let mut tasks = Vec::new();
        if let Some(number) = stale {
            state.selected = None;
            tasks.push(self.select(key, view, number));
        }
        if again {
            tasks.push(self.ask_docs(key, view));
        }
        Task::batch(tasks)
    }

    /// Shows a ticket in the popout, its texts asked for now.
    pub(super) fn select(&mut self, key: &RunnerKey, view: u64, number: u64) -> Task<Message> {
        let Some(state) = self.plans.get_mut(&view) else {
            return Task::none();
        };
        if state.selected == Some(number) && state.shown_error.is_none() {
            return Task::none();
        }
        state.selected = Some(number);
        // The ticket shown stays until the new answer replaces it, so a
        // refresh does not flicker.
        if state.shown.as_ref().is_some_and(|s| s.number != number) {
            state.shown = None;
        }
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
                // A ticket's note is said in its action row; an onboarding
                // request's text says what it asks for.
                let shown_text = match doc.kind {
                    DocKind::Onboarding => doc.shown_text(),
                    DocKind::Ticket => doc.body.clone(),
                };
                let body = markdown::Content::parse(&shown_text);
                // Each section's text without its heading, which its card
                // draws itself.
                let sections = super::review::sections(&doc.plan_text)
                    .into_iter()
                    .map(|section| {
                        let slice = &doc.plan_text[section.range.clone()];
                        let body = if section.level > 0 {
                            slice.split_once('\n').map_or("", |(_, rest)| rest)
                        } else {
                            slice
                        };
                        (section, markdown::Content::parse(body))
                    })
                    .collect();
                state.shown = Some(Shown {
                    number,
                    doc: *doc,
                    body,
                    sections,
                });
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
        state.acting_on = acting_on(&request);
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
            state.acting_on = None;
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

/// The document a request acts on and the word for what it does.
fn acting_on(request: &CiRequest) -> Option<(u64, &'static str)> {
    match request {
        CiRequest::DocState { number, state, .. } => {
            let verb = match state {
                DocState::Ready => "releasing",
                DocState::Open => "holding",
                DocState::Approved => "approving",
                DocState::Review => "unqueueing",
                DocState::Done => "finishing",
                DocState::Dismissed => "dismissing",
                DocState::Rejected => "rejecting",
                _ => return None,
            };
            Some((*number, verb))
        }
        CiRequest::DocDelete { number, .. } => Some((*number, "deleting")),
        CiRequest::DocRefine { number, .. } => Some((*number, "refining")),
        CiRequest::DocRunsOn { number, .. }
        | CiRequest::DocEdit { number, .. }
        | CiRequest::DocLinks { number, .. } => Some((*number, "saving")),
        _ => None,
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

/// The number of the ticket the CI stored.
pub(super) async fn create(endpoint: Endpoint, request: CiRequest) -> Result<u64, String> {
    match feed::ci(endpoint, request).await? {
        CiReply::DocCreated { number } => Ok(number),
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
pub(super) fn actions(doc: &DocView) -> Vec<(char, &'static str, &'static str, Option<DocState>)> {
    let dismiss = (
        glyph::CLOSE,
        "Dismiss",
        "It will not be done",
        Some(DocState::Dismissed),
    );
    match (doc.kind, doc.state) {
        (DocKind::Ticket, DocState::Open) => vec![
            (
                EDIT,
                "Plan",
                "Release it for planning: the CI opens a plan session once its dependencies are \
                 done",
                Some(DocState::Ready),
            ),
            dismiss,
        ],
        (DocKind::Ticket, DocState::Ready) => vec![
            (
                glyph::CLOSE,
                "Hold",
                "Back to open: no plan session starts",
                Some(DocState::Open),
            ),
            dismiss,
        ],
        (DocKind::Ticket, DocState::Review) => vec![
            (
                CHECK,
                "Approve",
                "Queue its plan for a master on a runner it fits",
                Some(DocState::Approved),
            ),
            (
                TRASH,
                "Discard plan",
                "Back to open without its plan; the tickets it includes are open again",
                Some(DocState::Open),
            ),
            dismiss,
        ],
        (DocKind::Ticket, DocState::Approved) => vec![
            (
                glyph::RERUN,
                "Back to review",
                "Take it out of the queue",
                Some(DocState::Review),
            ),
            dismiss,
        ],
        (DocKind::Ticket, DocState::InProgress) => vec![
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
        // The CI moves it on by itself.
        (DocKind::Ticket, DocState::Planning) => Vec::new(),
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
    pub(in crate::app) fn runner_key(&self, name: &str) -> Option<RunnerKey> {
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

    /// Asks the CI for a plan session of the Board's repository without a
    /// ticket, on this editor's own runner when it hosts sessions; its plan
    /// becomes a new ticket.
    pub(super) fn open_session(&mut self, key: &RunnerKey, view: u64) -> Task<Message> {
        let Some(client) = self.ci.get(key) else {
            return Task::none();
        };
        let runner = self.own_session_runner(client);
        let Some(state) = client.plans.get(&view) else {
            return Task::none();
        };
        let request = CiRequest::OpenSession {
            repo: state.repo.clone(),
            runner,
        };
        let endpoint = client.endpoint.clone();
        self.opening_session = Some((key.clone(), state.repo.clone()));
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
        self.opening_session = None;
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

    /// A repository's Board toolbar: its name, what the last action came
    /// to, `Plan session` and `Pause queue`.
    pub(super) fn plans_toolbar<'a>(
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
            text("Board").size(TEXT_MD),
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
        let mut pressed = action_button(EDIT, "New ticket");
        pressed = pressed.on_press_with(move || {
            Message::Ticket(TicketMsg::New {
                ci: runner.clone(),
                view,
            })
        });
        toolbar = toolbar.push(tip(
            pressed,
            "Write a ticket for this repository".to_owned(),
        ));
        let possible = client.sessions_possible();
        let mut pressed = action_button(EDIT, "Plan session");
        if possible {
            pressed = pressed
                .on_press_with(move || Message::Ci(runner.clone(), CiMsg::OpenSession { view }));
        }
        let what = if possible {
            "omp in plan mode on a session runner, through the CI; its plan becomes a new ticket"
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
                PLAY,
                "Resume queue",
                "Dispatch the approved plans to runners again",
            )
        } else {
            (
                PAUSE,
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
        toolbar.into()
    }

    /// Whether the review being written of ticket `doc` of CI `ci` has
    /// anything to send: a deletion, a note or overall feedback.
    pub(super) fn review_pending(&self, ci: &RunnerKey, doc: &DocView) -> bool {
        self.reviews
            .get(&(ci.clone(), doc.repo.clone(), doc.number))
            .is_some_and(|draft| draft.revision == doc.revision && !draft.is_empty())
    }

    /// The buttons a ticket's state allows, the primary one filled, and what
    /// it waits for: `Open session` for a ticket a session holds, `Open` and
    /// `Stop` while a master works on it, the state's actions, `Refine`
    /// beside `Approve` for a plan in review, `Edit` for an open or ready
    /// ticket, and the reason the CI last sent it back.
    pub(super) fn action_bar<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        view: u64,
        state: &'a PlansState,
        doc: &'a DocView,
    ) -> Element<'a, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let muted = self.theme.muted();
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
        let ticket = doc.kind == DocKind::Ticket;
        let open_or_ready = ticket && matches!(doc.state, DocState::Open | DocState::Ready);
        if open_or_ready
            && let Some(claim) = client.claim_on(&ClaimSubject::Ticket {
                repo: doc.repo.clone(),
                number: doc.number,
            })
        {
            let mut pressed = pill("Open session", true);
            if let Some((key, terminal)) = client.holder_target(claim) {
                pressed =
                    pressed.on_press_with(move || Message::RevealTerminal(key.clone(), terminal));
            }
            bar = bar.push(tip(pressed, claim.holder_text()));
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
        let in_review = ticket && doc.state == DocState::Review;
        let pending = in_review && self.review_pending(runner, doc);
        for (_, words, what, to) in actions(doc) {
            let held = words == "Approve" && pending;
            let primary = !primary_taken && !held && matches!(words, "Approve" | "Plan");
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
            if !state.acting && !held {
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
            let what = if held {
                "Your deletions and notes go back with Refine; Approve executes the plan as it is"
            } else {
                what
            };
            let key = match words {
                "Approve" => " (a)",
                "Dismiss" | "Reject" => " (r)",
                "Plan" => " (p)",
                _ => "",
            };
            bar = bar.push(tip(pressed, format!("{what}{key}")));
            if words == "Approve" && in_review {
                let mut refine = pill("Refine", pending);
                if pending && !state.acting {
                    refine = refine.on_press(Message::Ticket(TicketMsg::Refine));
                }
                let what = if pending {
                    "Send your review to the plan session, which revises the plan"
                } else {
                    "Delete or annotate a part, or write overall feedback"
                };
                bar = bar.push(tip(refine, what.to_owned()));
            }
        }
        if open_or_ready {
            let mut edit = pill("Edit", false);
            if !state.acting {
                edit = edit.on_press(Message::Ticket(TicketMsg::Edit));
            }
            bar = bar.push(tip(edit, "Change its title and proposal".to_owned()));
        }
        let mut waits = Vec::new();
        if matches!(doc.state, DocState::Ready | DocState::Approved) {
            let blocked: Vec<String> = blocked_by(doc, state.docs())
                .iter()
                .map(|n| format!("#{n}"))
                .collect();
            if !blocked.is_empty() {
                waits.push(format!("waits for {}", blocked.join(", ")));
            }
        }
        if doc.state == DocState::Ready
            && let Some(planner) = &doc.planner
            && !client.hosts_sessions(&planner.runner)
        {
            waits.push(format!(
                "waits for {}, where its plan session ran",
                planner.runner
            ));
        }
        if doc.state == DocState::Approved && client.paused(&doc.repo) {
            waits.push("queue paused".to_owned());
        }
        for wait in waits {
            bar = bar.push(text(wait).size(TEXT_SM).color(muted));
        }
        if !doc.note.is_empty() {
            bar = bar.push(
                text(doc.note.as_str())
                    .size(TEXT_SM)
                    .color(self.theme.error()),
            );
        }
        bar.wrap().vertical_spacing(SP_2).into()
    }

    /// The `Runs on` of a ticket: a field while it can change (open, ready,
    /// in review or approved), else a line when it names any.
    pub(super) fn runs_on_line<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a PlansState,
        doc: &'a DocView,
    ) -> Option<Element<'a, Message, Theme>> {
        let editable = doc.kind == DocKind::Ticket
            && matches!(
                doc.state,
                DocState::Open | DocState::Ready | DocState::Review | DocState::Approved
            );
        if !editable {
            if doc.runs_on.is_empty() {
                return None;
            }
            return Some(text(doc.runs_on.join("; ")).size(TEXT_SM).into());
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
        Some(
            text_input("any runner, or os=windows; os=linux", &value)
                .size(TEXT_SM)
                .on_input(move |text| {
                    Message::Ci(runner.clone(), CiMsg::RunsOnEdited { view, number, text })
                })
                .on_submit(Message::Ci(runner.clone(), CiMsg::DocAct { view, request }))
                .into(),
        )
    }

    /// `a`, `r` and `p` on ticket `number` of Plans `view`: approve,
    /// dismiss (reject an onboarding request) or release it for planning,
    /// each only where the ticket offers it. A plan with a review being
    /// written is not approved: the review goes back with Refine.
    pub(super) fn plans_letter(
        &mut self,
        ci: &RunnerKey,
        view: u64,
        number: u64,
        shortcut: CiShortcut,
    ) -> Task<Message> {
        let Some(doc) = self
            .ci
            .get(ci)
            .and_then(|client| client.plans.get(&view))
            .and_then(|state| state.doc(number))
        else {
            return Task::none();
        };
        let wanted = match shortcut {
            CiShortcut::Approve => "Approve",
            CiShortcut::Rerun if doc.kind == DocKind::Onboarding => "Reject",
            CiShortcut::Rerun => "Dismiss",
            CiShortcut::Plan => "Plan",
            _ => return Task::none(),
        };
        if wanted == "Approve" && doc.state == DocState::Review && self.review_pending(ci, doc) {
            return Task::none();
        }
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

impl CiClient {
    /// Whether runner `name` is connected and hosts sessions.
    pub(super) fn hosts_sessions(&self, name: &str) -> bool {
        self.runner_view(name)
            .is_some_and(|m| m.sessions && m.status != "offline")
    }

    /// Whether the CI dispatches none of `repo`'s approved tickets.
    pub(super) fn paused(&self, repo: &str) -> bool {
        self.overview
            .as_ref()
            .and_then(|o| o.repos.iter().find(|r| r.repo == repo))
            .is_some_and(|r| r.paused)
    }
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
                    responsible: None,
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
                    open: 0,
                    planning: 0,
                    onboarding: None,
                })
                .collect(),
            build: 0,
            sessions: Vec::new(),
            claims: Vec::new(),
            claude: None,
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
