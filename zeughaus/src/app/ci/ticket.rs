//! The ticket popout: one ticket of a Board laid over the workspace, with
//! its proposal, its plan under review, its links, its sessions and what
//! its state lets the user do; or a new ticket being written. The palette
//! and the notice center lie over it; Escape or a press on the scrim closes
//! it.

use iced::widget::{
    Space, button, column, container, markdown, mouse_area, opaque, row, rule, text, text_editor,
};
use iced::{Alignment, Border, Element, Font, Length, Shadow, Task, Vector};
use iced_tabs::scroll;
use zeughaus_link::{
    AgentRole, CiRequest, ClaimSubject, DocKind, DocState, DocView, LinkKind, MAX_COMMENT_BYTES,
    MAX_COMMENTS, SessionKind, TicketSuggestion, Verdict, compose_text, linked_from,
    proposal_heading, ticket_text,
};
use zeughaus_mux::TerminalId;
use zeughaus_theme::Theme;

use super::plans::{BODY_MAX_WIDTH, EDIT, PlansState, Shown, TRASH, create};
use super::review::{ReviewDraft, Section, deleted_at, inside};
use super::{
    CiClient, Notice, SP_1, SP_2, SP_3, SP_4, TEXT_MD, TEXT_SM, Tone, ago, glyph, icon,
    icon_button, now, tip,
};
use crate::app::App;
use crate::app::agents::role_glyph;
use crate::message::{CiMsg, Message, TicketMsg, TicketTab};
use crate::style::{self, Tokens};
use crate::workspace::RunnerKey;

/// The widest the popout grows.
const DIALOG_MAX_WIDTH: f32 = 1100.0;
/// The side column of links, sessions and state, and the panel beside a
/// ticket's editor.
const SIDE_WIDTH: f32 = 280.0;
/// The proposal's editor.
const EDITOR_HEIGHT: f32 = 400.0;
/// The overall feedback of a review.
const OVERALL_HEIGHT: f32 = 80.0;
/// An agent's open ask over the tabs, past which it scrolls.
const ASK_MAX_HEIGHT: f32 = 320.0;
/// The most sessions the side column lists.
const SESSIONS_SHOWN: usize = 5;
/// The longest linked title a chip shows, in characters.
const CHIP_TITLE: usize = 40;

/// The popout over the workspace and what is being typed in it.
pub(in crate::app) struct TicketPopout {
    pub(super) ci: RunnerKey,
    /// The Board's Plans view.
    pub(super) view: u64,
    /// `None` while a new ticket is written.
    pub(super) number: Option<u64>,
    tab: TicketTab,
    /// The text being edited.
    editing: Option<Editing>,
    /// The new ticket being written.
    creating: Option<Editing>,
    /// What an editor shows before its review exists: the first action on
    /// it makes the review.
    blank: text_editor::Content,
    /// The comment being written.
    pub(super) comment: text_editor::Content,
    /// A comment is on its way: the CI's answer empties the box.
    pub(super) comment_sent: bool,
}

impl TicketPopout {
    fn new(ci: RunnerKey, view: u64, number: Option<u64>, tab: TicketTab) -> TicketPopout {
        TicketPopout {
            ci,
            view,
            number,
            tab,
            editing: None,
            creating: None,
            blank: text_editor::Content::new(),
            comment: text_editor::Content::new(),
            comment_sent: false,
        }
    }
}

/// A ticket's Markdown text being typed: its title and links are read
/// from it.
struct Editing {
    body: text_editor::Content,
}

/// A title cut to `CHIP_TITLE` characters.
fn shortened(title: &str) -> String {
    if title.chars().count() <= CHIP_TITLE {
        return title.to_owned();
    }
    let cut: String = title.chars().take(CHIP_TITLE - 1).collect();
    format!("{cut}\u{2026}")
}

/// How a ticket's state is coloured.
fn state_tone(state: DocState) -> Tone {
    match state {
        DocState::Ready | DocState::Planning | DocState::InProgress => Tone::Active,
        DocState::Review => Tone::Wait,
        DocState::Done => Tone::Good,
        _ => Tone::Idle,
    }
}

/// The glyph of a linked ticket's state.
fn state_glyph(state: DocState) -> char {
    match state {
        DocState::Done => glyph::PASS,
        DocState::Dismissed | DocState::Rejected => glyph::CANCELLED,
        DocState::Planning | DocState::InProgress => glyph::RUNNING,
        DocState::Review | DocState::Approved => glyph::PENDING,
        _ => glyph::DOT,
    }
}

impl App {
    /// The shown ticket of the popout, once the CI has sent it.
    fn popout_shown(&self) -> Option<(&TicketPopout, &Shown)> {
        let popout = self.ticket.as_ref()?;
        let number = popout.number?;
        let shown = self
            .ci
            .get(&popout.ci)?
            .plans
            .get(&popout.view)?
            .shown
            .as_ref()
            .filter(|shown| shown.number == number)?;
        Some((popout, shown))
    }

    /// The review of the shown plan, made fresh for a new revision.
    fn popout_draft(&mut self) -> Option<&mut ReviewDraft> {
        let (key, revision) = {
            let (popout, shown) = self.popout_shown()?;
            (
                (popout.ci.clone(), shown.doc.repo.clone(), shown.number),
                shown.doc.revision,
            )
        };
        let draft = self
            .reviews
            .entry(key)
            .or_insert_with(|| ReviewDraft::new(revision));
        if draft.revision != revision {
            *draft = ReviewDraft::new(revision);
        }
        Some(draft)
    }

    /// Says `text` as the Board's notice, which the popout shows too.
    fn ticket_notice(&mut self, text: String) {
        if let Some(popout) = &self.ticket
            && let Some(state) = self
                .ci
                .get_mut(&popout.ci)
                .and_then(|client| client.plans.get_mut(&popout.view))
        {
            state.notice = Some(Notice { text, failed: true });
        }
    }

    /// Sends `request` for the popout's ticket, as the Board's actions do.
    fn ticket_act(&mut self, request: CiRequest) -> Task<Message> {
        let Some(popout) = &self.ticket else {
            return Task::none();
        };
        let (ci, view) = (popout.ci.clone(), popout.view);
        self.apply_ci(ci, CiMsg::DocAct { view, request })
    }

    /// Opens ticket `number` of `repo` in the popout, on the first CI that
    /// lists the repository.
    pub(in crate::app) fn show_ticket(&mut self, repo: &str, number: u64) -> Task<Message> {
        let found = self
            .ci
            .iter()
            .find_map(|(key, client)| Some((key.clone(), client.plans_view_of(repo)?)));
        let Some((ci, view)) = found else {
            self.hint = Some((
                format!("#{repo}-{number}: no CI lists the repository {repo}"),
                iced::time::Instant::now(),
            ));
            return Task::none();
        };
        self.apply_ticket(TicketMsg::Open { ci, view, number })
    }

    /// What the popout's messages do.
    pub(in crate::app) fn apply_ticket(&mut self, message: TicketMsg) -> Task<Message> {
        match message {
            TicketMsg::Open { ci, view, number } => {
                let planned = self
                    .ci
                    .get(&ci)
                    .and_then(|client| client.plans.get(&view))
                    .and_then(|state| state.doc(number))
                    .is_some_and(|doc| doc.revision > 0);
                let tab = if planned {
                    TicketTab::Plan
                } else {
                    TicketTab::Proposal
                };
                if self.palette_open {
                    self.palette_close();
                }
                self.ticket = Some(TicketPopout::new(ci.clone(), view, Some(number), tab));
                match self.ci.get_mut(&ci) {
                    Some(client) => client.select(&ci, view, number),
                    None => Task::none(),
                }
            }
            TicketMsg::New { ci, view } => {
                if self.palette_open {
                    self.palette_close();
                }
                let mut popout = TicketPopout::new(ci, view, None, TicketTab::Proposal);
                popout.creating = Some(Editing {
                    body: text_editor::Content::new(),
                });
                self.ticket = Some(popout);
                Task::none()
            }
            TicketMsg::Close => {
                self.ticket = None;
                Task::none()
            }
            TicketMsg::Tab(tab) => {
                if let Some(popout) = &mut self.ticket {
                    popout.tab = tab;
                }
                Task::none()
            }
            TicketMsg::Edit => {
                // A ticket that started as a plan has no text yet: it starts
                // from its title and links.
                let editing = self.popout_shown().map(|(_, shown)| {
                    let doc = &shown.doc;
                    let text = if doc.body.trim().is_empty() {
                        compose_text(&doc.title, &doc.links, "")
                    } else {
                        doc.body.clone()
                    };
                    Editing {
                        body: text_editor::Content::with_text(&text),
                    }
                });
                if let (Some(popout), Some(editing)) = (&mut self.ticket, editing) {
                    popout.tab = TicketTab::Proposal;
                    popout.editing = Some(editing);
                }
                Task::none()
            }
            TicketMsg::EditBody(action) => {
                if let Some(popout) = &mut self.ticket
                    && let Some(editing) = popout.editing.as_mut().or(popout.creating.as_mut())
                {
                    editing.body.perform(action);
                }
                Task::none()
            }
            TicketMsg::EditSave => {
                let Some((popout, shown)) = self.popout_shown() else {
                    return Task::none();
                };
                let Some(editing) = &popout.editing else {
                    return Task::none();
                };
                let request = CiRequest::DocEdit {
                    repo: shown.doc.repo.clone(),
                    number: shown.number,
                    body: editing.body.text(),
                };
                if let Some(popout) = &mut self.ticket {
                    popout.editing = None;
                }
                self.ticket_act(request)
            }
            TicketMsg::EditCancel => {
                if let Some(popout) = &mut self.ticket {
                    popout.editing = None;
                    if popout.creating.is_some() {
                        self.ticket = None;
                    }
                }
                Task::none()
            }
            TicketMsg::PreApprove(on) => {
                let Some(doc) = self.popout_doc() else {
                    return Task::none();
                };
                let request = CiRequest::DocAutoApprove {
                    repo: doc.repo.clone(),
                    number: doc.number,
                    auto_approve: on,
                };
                self.ticket_act(request)
            }
            TicketMsg::Create => {
                let Some(popout) = &self.ticket else {
                    return Task::none();
                };
                let Some(creating) = &popout.creating else {
                    return Task::none();
                };
                let Some((client, state)) = self
                    .ci
                    .get(&popout.ci)
                    .and_then(|client| Some((client, client.plans.get(&popout.view)?)))
                else {
                    return Task::none();
                };
                let request = CiRequest::DocCreate {
                    repo: state.repo.clone(),
                    kind: DocKind::Ticket,
                    body: creating.body.text(),
                    source: "user".to_owned(),
                    runs_on: Vec::new(),
                    onboard: None,
                };
                Task::perform(create(client.endpoint.clone(), request), |result| {
                    Message::Ticket(TicketMsg::Created(result))
                })
            }
            TicketMsg::Created(Ok(number)) => {
                let Some(popout) = &mut self.ticket else {
                    return Task::none();
                };
                popout.number = Some(number);
                popout.creating = None;
                popout.tab = TicketTab::Proposal;
                let (ci, view) = (popout.ci.clone(), popout.view);
                match self.ci.get_mut(&ci) {
                    Some(client) => {
                        Task::batch([client.ask_docs(&ci, view), client.select(&ci, view, number)])
                    }
                    None => Task::none(),
                }
            }
            TicketMsg::Created(Err(e)) => {
                self.ticket_notice(e);
                Task::none()
            }
            TicketMsg::Collapse(at) => {
                if let Some(draft) = self.popout_draft()
                    && !draft.collapsed.remove(&at)
                {
                    draft.collapsed.insert(at);
                }
                Task::none()
            }
            TicketMsg::Delete(at) => {
                if let Some(draft) = self.popout_draft()
                    && !draft.deleted.remove(&at)
                {
                    draft.deleted.insert(at);
                }
                Task::none()
            }
            TicketMsg::Annotate(at) => {
                if let Some(draft) = self.popout_draft() {
                    if !draft.open_notes.remove(&at) {
                        draft.open_notes.insert(at);
                    }
                    draft.notes.entry(at).or_default();
                }
                Task::none()
            }
            TicketMsg::Note(at, action) => {
                if let Some(draft) = self.popout_draft() {
                    draft.notes.entry(at).or_default().perform(action);
                }
                Task::none()
            }
            TicketMsg::Overall(action) => {
                if let Some(draft) = self.popout_draft() {
                    draft.overall.perform(action);
                }
                Task::none()
            }
            TicketMsg::Refine => {
                let Some((popout, shown)) = self.popout_shown() else {
                    return Task::none();
                };
                let key = (popout.ci.clone(), shown.doc.repo.clone(), shown.number);
                let sections: Vec<Section> =
                    shown.sections.iter().map(|(s, _)| s.clone()).collect();
                let Some(draft) = self
                    .reviews
                    .get(&key)
                    .filter(|draft| draft.revision == shown.doc.revision)
                else {
                    return Task::none();
                };
                let feedback = draft.text(&sections);
                if feedback.is_empty() {
                    return Task::none();
                }
                let request = CiRequest::DocRefine {
                    repo: key.1,
                    number: key.2,
                    feedback,
                };
                self.ticket_act(request)
            }
            TicketMsg::CommentBody(action) => {
                if let Some(popout) = &mut self.ticket {
                    popout.comment.perform(action);
                }
                Task::none()
            }
            TicketMsg::Comment => {
                let Some((popout, shown)) = self.popout_shown() else {
                    return Task::none();
                };
                let text = popout.comment.text();
                if text.trim().is_empty() {
                    return Task::none();
                }
                let request = CiRequest::DocComment {
                    repo: shown.doc.repo.clone(),
                    number: shown.number,
                    author: zeughaus_link::manual::actor(),
                    text,
                };
                if let Some(popout) = &mut self.ticket {
                    popout.comment_sent = true;
                }
                self.ticket_act(request)
            }
            TicketMsg::EditSuggested => {
                let editing = self.popout_shown().and_then(|(_, shown)| {
                    shown
                        .doc
                        .suggestion
                        .as_ref()
                        .filter(|s| s.verdict == Verdict::Rewrite)
                        .map(|s| Editing {
                            body: text_editor::Content::with_text(&s.text),
                        })
                });
                if let (Some(popout), Some(editing)) = (&mut self.ticket, editing) {
                    popout.tab = TicketTab::Proposal;
                    popout.editing = Some(editing);
                }
                Task::none()
            }
        }
    }

    /// The popout's ticket as the list has it.
    fn popout_doc(&self) -> Option<&DocView> {
        let popout = self.ticket.as_ref()?;
        self.ci
            .get(&popout.ci)?
            .plans
            .get(&popout.view)?
            .doc(popout.number?)
    }

    /// The popout over the workspace, when one is open.
    pub(in crate::app) fn ticket_view(&self) -> Option<Element<'_, Message, Theme>> {
        let popout = self.ticket.as_ref()?;
        let client = self.ci.get(&popout.ci)?;
        let state = client.plans.get(&popout.view)?;
        let dialog = match popout.number {
            None => self.new_ticket_dialog(popout, client, state),
            Some(number) => self.ticket_dialog(popout, client, state, number),
        };
        let panel = container(dialog)
            .width(Length::Fill)
            .max_width(DIALOG_MAX_WIDTH)
            .height(Length::Fill)
            .padding(SP_4)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.extended().background.base.color.into()),
                border: Border {
                    color: theme.extended().background.strong.color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow {
                    color: iced::Color::BLACK.scale_alpha(0.35),
                    offset: Vector::new(0.0, 4.0),
                    blur_radius: 16.0,
                },
                ..Default::default()
            });
        // `opaque` keeps a press inside the dialog from reaching the scrim,
        // and the scrim keeps the pointer from the workspace under it.
        Some(opaque(
            mouse_area(
                container(opaque(panel))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(24)
                    .center_x(Length::Fill)
                    .style(|theme: &Theme| container::Style {
                        background: Some(
                            theme
                                .extended()
                                .background
                                .base
                                .color
                                .scale_alpha(0.6)
                                .into(),
                        ),
                        ..Default::default()
                    }),
            )
            .on_press(Message::Ticket(TicketMsg::Close)),
        ))
    }

    /// Markdown in the editor's face, with theme roles for inline code.
    pub(super) fn markdown_settings(&self) -> markdown::Settings {
        let palette = self.theme.base().palette();
        let mut style = markdown::Style::from_palette(palette);
        style.font = crate::font::UI;
        style.inline_code_font = crate::font::UI;
        style.code_block_font = crate::font::UI;
        style.inline_code_highlight.background = Tokens::of(&self.theme).surface_hover.into();
        style.inline_code_color = palette.text;
        markdown::Settings::with_text_size(TEXT_MD, style)
    }

    /// The close button of the popout's corner.
    fn close_button(&self) -> Element<'_, Message, Theme> {
        tip(
            icon_button(glyph::CLOSE, None)
                .width(32)
                .height(32)
                .on_press(Message::Ticket(TicketMsg::Close)),
            "Close (Esc)".to_owned(),
        )
    }

    /// A pill: the primary one filled.
    pub(super) fn ticket_pill<'a>(
        &self,
        words: impl iced::widget::text::IntoFragment<'a>,
        primary: bool,
    ) -> button::Button<'a, Message, Theme> {
        let t = Tokens::of(&self.theme);
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
    }

    /// A new ticket: its Markdown text beside what is read from it, Create
    /// and Cancel.
    fn new_ticket_dialog<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let Some(creating) = &popout.creating else {
            return text("Storing.").size(TEXT_SM).color(muted).into();
        };
        let header = row![
            text(format!("New ticket of {}", state.repo)).size(20),
            Space::new().width(Length::Fill),
            self.close_button(),
        ]
        .spacing(SP_3)
        .align_y(Alignment::Center);
        let typed = creating.body.text();
        let body = text_editor(&creating.body)
            .placeholder(
                "# Title\nDepends on: #12, #weida-3\n\nWhat the ticket asks for, in Markdown",
            )
            .height(EDITOR_HEIGHT)
            .on_action(|action| Message::Ticket(TicketMsg::EditBody(action)));
        let mut create = self.ticket_pill("Create", true);
        if ticket_text(&typed).is_ok() {
            create = create.on_press(Message::Ticket(TicketMsg::Create));
        }
        let mut actions = row![
            create,
            self.ticket_pill("Cancel", false)
                .on_press(Message::Ticket(TicketMsg::EditCancel)),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center);
        if let Some(notice) = &state.notice {
            actions = actions.push(
                text(notice.text.as_str())
                    .size(TEXT_SM)
                    .color(self.theme.error()),
            );
        }
        let editor = row![
            container(body).width(Length::Fill),
            rule::vertical(1),
            container(self.text_panel(popout, client, state, &typed)).width(SIDE_WIDTH),
        ]
        .spacing(SP_3);
        column![header, editor, actions].spacing(SP_3).into()
    }

    /// What a ticket's text says, beside its editor: its title and links,
    /// or why the CI would refuse it.
    fn text_panel<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
        body: &str,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let block =
            |label: &'a str, content: Element<'a, Message, Theme>| -> Element<'a, Message, Theme> {
                column![text(label).size(TEXT_SM).color(muted), content]
                    .spacing(SP_1)
                    .into()
            };
        let parsed = match ticket_text(body) {
            Ok(parsed) => parsed,
            Err(e) => {
                return column![text(e).size(TEXT_SM).color(self.theme.error())]
                    .padding([0.0, SP_2])
                    .into();
            }
        };
        let mut panel = column![block("Title", text(parsed.title).size(TEXT_SM).into())]
            .spacing(SP_3)
            .padding([0.0, SP_2]);
        for (label, kind) in [
            ("Depends on", LinkKind::DependsOn),
            ("Related", LinkKind::Related),
        ] {
            let mut chips = column![].spacing(SP_1);
            let mut any = false;
            for link in parsed.links.iter().filter(|link| link.kind == kind) {
                let repo = link.repo.as_deref().unwrap_or(&state.repo);
                chips = chips.push(self.ref_chip(
                    popout,
                    client,
                    state,
                    &state.repo,
                    repo,
                    link.number,
                ));
                any = true;
            }
            if !any {
                chips = chips.push(text("none").size(TEXT_SM).color(muted));
            }
            panel = panel.push(block(label, chips.into()));
        }
        panel.into()
    }

    /// One ticket: header, actions, tabs, then its proposal or plan beside
    /// the side column.
    fn ticket_dialog<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
        number: u64,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let shown = state.shown.as_ref().filter(|shown| shown.number == number);
        // The list is the fresher of the two; the shown ticket has the texts.
        let Some(doc) = state.doc(number).or(shown.map(|shown| &shown.doc)) else {
            let words = state
                .shown_error
                .clone()
                .unwrap_or_else(|| "Loading.".to_owned());
            return column![
                row![Space::new().width(Length::Fill), self.close_button()],
                text(words).size(TEXT_SM).color(muted),
            ]
            .into();
        };
        let t = Tokens::of(&self.theme);
        let title = text(doc.title.as_str()).size(20).width(Length::Fill);
        let tone = state_tone(doc.state);
        let chip = container(
            text(doc.state.name().replace('_', " "))
                .size(TEXT_SM)
                .color(tone.text(&self.theme)),
        )
        .padding([0, 6])
        .style(style::badge(&t, false));
        let header = row![
            text(format!("#{number}")).size(TEXT_SM).color(muted),
            container(title).width(Length::Fill),
            chip,
            self.close_button(),
        ]
        .spacing(SP_3)
        .align_y(Alignment::Center);
        let mut top = column![header].spacing(SP_3);
        if popout.editing.is_none() {
            top = top.push(self.action_bar(&popout.ci, client, popout.view, state, doc));
        }
        if let Some(suggestion) = shown.and_then(|shown| shown.doc.suggestion.as_ref())
            && doc.state.reevaluable()
        {
            top = top.push(self.suggestion_part(popout, client, state, doc, suggestion));
        }
        if let Some(notice) = &state.notice {
            let color = if notice.failed {
                self.theme.error()
            } else {
                muted
            };
            top = top.push(text(notice.text.as_str()).size(TEXT_SM).color(color));
        }
        if let Some(ask) = client
            .asks()
            .find(|ask| ask.repo == doc.repo && ask.doc == Some(number))
        {
            top = top.push(
                container(scroll::vertical(
                    container(self.ask_form(&popout.ci, client, ask)).padding(SP_3),
                ))
                .max_height(ASK_MAX_HEIGHT)
                .style(style::card(&t)),
            );
        }
        let mut tabs = row![].spacing(SP_2);
        let proposal = popout.tab == TicketTab::Proposal;
        tabs = tabs.push(
            self.ticket_pill("Proposal", proposal)
                .on_press(Message::Ticket(TicketMsg::Tab(TicketTab::Proposal))),
        );
        if doc.revision > 0 {
            tabs = tabs.push(
                self.ticket_pill(format!("Plan v{}", doc.revision), !proposal)
                    .on_press(Message::Ticket(TicketMsg::Tab(TicketTab::Plan))),
            );
        }
        top = top.push(tabs);
        let main: Element<'a, Message, Theme> = match (popout.tab, shown) {
            (_, None) => {
                let words = state.shown_error.as_deref().unwrap_or("Loading.");
                text(words).size(TEXT_SM).color(muted).into()
            }
            (TicketTab::Proposal, Some(shown)) => self.proposal_part(popout, client, state, shown),
            (TicketTab::Plan, Some(shown)) => self.plan_part(popout, client, state, doc, shown),
        };
        let main = match shown {
            Some(shown) if doc.kind == DocKind::Ticket => {
                column![main, self.comments_part(popout, state, shown)]
                    .spacing(SP_4)
                    .into()
            }
            _ => main,
        };
        let main = scroll::vertical(
            container(main)
                .padding([0.0, SP_3])
                .max_width(BODY_MAX_WIDTH),
        )
        .width(Length::Fill)
        .height(Length::Fill);
        let side = scroll::vertical(self.side_column(popout, client, state, doc))
            .width(SIDE_WIDTH)
            .height(Length::Fill);
        column![
            top,
            rule::horizontal(1),
            row![main, rule::vertical(1), side]
                .spacing(SP_3)
                .height(Length::Fill),
        ]
        .spacing(SP_3)
        .height(Length::Fill)
        .into()
    }

    /// What the advice model last suggested: its verdict, the ticket a
    /// merge goes into, who said it and when, and why.
    fn suggestion_part<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
        doc: &'a DocView,
        suggestion: &'a TicketSuggestion,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut line = row![
            text(format!("Suggested: {}", suggestion.headline()))
                .size(TEXT_SM)
                .color(self.theme.accent())
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center);
        if suggestion.verdict == Verdict::Merge
            && let Some(into) = suggestion.into
        {
            line = line.push(self.ref_chip(popout, client, state, &doc.repo, &doc.repo, into));
        }
        line = line.push(
            text(format!(
                "{} \u{b7} {}",
                suggestion.model,
                ago(suggestion.created, now())
            ))
            .size(TEXT_SM)
            .color(muted),
        );
        if suggestion.verdict == Verdict::Rewrite && popout.editing.is_none() {
            line = line.push(
                self.ticket_pill("Edit with this text", false)
                    .on_press(Message::Ticket(TicketMsg::EditSuggested)),
            );
        }
        column![line.wrap(), text(suggestion.reason.as_str()).size(TEXT_SM)]
            .spacing(SP_1)
            .into()
    }

    /// The ticket's comments, oldest first, and the box a new one is
    /// written in.
    fn comments_part<'a>(
        &'a self,
        popout: &'a TicketPopout,
        state: &'a PlansState,
        shown: &'a Shown,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let comments = &shown.doc.comments;
        let mut part = column![
            rule::horizontal(1),
            text(format!("Comments ({})", comments.len())).size(TEXT_MD),
        ]
        .spacing(SP_3);
        let at = now();
        for comment in comments {
            part = part.push(
                column![
                    text(format!("{} \u{b7} {}", comment.author, ago(comment.at, at)))
                        .size(TEXT_SM)
                        .color(muted),
                    text(comment.text.as_str()).size(TEXT_SM),
                ]
                .spacing(SP_1),
            );
        }
        if comments.len() >= MAX_COMMENTS {
            return part
                .push(
                    text(format!("{MAX_COMMENTS} comments: it takes no more"))
                        .size(TEXT_SM)
                        .color(muted),
                )
                .into();
        }
        let typed = popout.comment.text();
        let mut send = self.ticket_pill("Comment", false);
        if !typed.trim().is_empty() && typed.len() <= MAX_COMMENT_BYTES && !state.acting {
            send = send.on_press(Message::Ticket(TicketMsg::Comment));
        }
        part.push(
            text_editor(&popout.comment)
                .placeholder("A comment: the next plan session and a Refine read it")
                .height(OVERALL_HEIGHT)
                .on_action(|action| Message::Ticket(TicketMsg::CommentBody(action))),
        )
        .push(send)
        .into()
    }

    /// The Proposal tab: the proposal, or its editor beside what is read
    /// from it.
    fn proposal_part<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
        shown: &'a Shown,
    ) -> Element<'a, Message, Theme> {
        if let Some(editing) = &popout.editing {
            let typed = editing.body.text();
            let body = text_editor(&editing.body)
                .height(EDITOR_HEIGHT)
                .on_action(|action| Message::Ticket(TicketMsg::EditBody(action)));
            let mut save = self.ticket_pill("Save", true);
            if ticket_text(&typed).is_ok() {
                save = save.on_press(Message::Ticket(TicketMsg::EditSave));
            }
            let buttons = row![
                save,
                self.ticket_pill("Cancel", false)
                    .on_press(Message::Ticket(TicketMsg::EditCancel)),
            ]
            .spacing(SP_2);
            let body = row![
                container(body).width(Length::Fill),
                rule::vertical(1),
                container(self.text_panel(popout, client, state, &typed)).width(SIDE_WIDTH),
            ]
            .spacing(SP_3);
            return column![body, buttons].spacing(SP_2).into();
        }
        let rest = zeughaus_link::text_without_title(&shown.doc.body);
        if rest.trim().is_empty() && shown.doc.onboard.is_none() {
            let words = if shown.doc.body.trim().is_empty() {
                "No proposal text: this ticket started as a plan."
            } else {
                "No text beyond its title."
            };
            return text(words).size(TEXT_SM).color(self.theme.muted()).into();
        }
        let ci = popout.ci.clone();
        markdown::view(shown.body.items(), self.markdown_settings())
            .map(move |uri| Message::Ci(ci.clone(), CiMsg::OpenLink(uri)))
    }

    /// The Plan tab: the plan in its sections, which are deleted and
    /// annotated while the plan is in review.
    fn plan_part<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
        doc: &'a DocView,
        shown: &'a Shown,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let review = doc.kind == DocKind::Ticket && doc.state == DocState::Review;
        let draft = self
            .reviews
            .get(&(popout.ci.clone(), shown.doc.repo.clone(), shown.number))
            .filter(|draft| draft.revision == shown.doc.revision);
        let sections: Vec<Section> = shown.sections.iter().map(|(s, _)| s.clone()).collect();
        let mut part = column![].spacing(SP_3);
        if doc.state == DocState::Planning && doc.revision > 0 {
            part = part.push(
                text("A plan session is revising this plan")
                    .size(TEXT_SM)
                    .color(self.theme.accent()),
            );
        }
        if review {
            let overall = draft.map_or(&popout.blank, |draft| &draft.overall);
            part = part.push(
                column![
                    text("Overall feedback").size(TEXT_SM).color(muted),
                    text_editor(overall)
                        .placeholder("What the whole plan should change")
                        .height(OVERALL_HEIGHT)
                        .on_action(|action| Message::Ticket(TicketMsg::Overall(action))),
                ]
                .spacing(SP_1),
            );
        }
        let collapsed = |at: usize| {
            draft.is_some_and(|draft| {
                draft
                    .collapsed
                    .iter()
                    .any(|&folded| inside(&sections, at, folded))
            })
        };
        for (at, (section, content)) in shown.sections.iter().enumerate() {
            if collapsed(at) {
                continue;
            }
            let gone = draft.is_some_and(|draft| deleted_at(&sections, &draft.deleted, at));
            let under_gone = draft.is_some_and(|draft| {
                draft
                    .deleted
                    .iter()
                    .any(|&other| inside(&sections, at, other))
            });
            let has_children = section.subtree_end > section.range.end;
            let mut head = row![].spacing(SP_2).align_y(Alignment::Center);
            if has_children {
                let folded = draft.is_some_and(|draft| draft.collapsed.contains(&at));
                let chevron = if folded {
                    glyph::CHEVRON_RIGHT
                } else {
                    glyph::CHEVRON_DOWN
                };
                head = head.push(
                    button(icon(chevron, TEXT_SM, muted))
                        .padding(0)
                        .style(super::link_style)
                        .on_press(Message::Ticket(TicketMsg::Collapse(at))),
                );
            }
            let size = if section.level <= 2 { TEXT_MD } else { TEXT_SM };
            let strong = Font {
                weight: iced::font::Weight::Bold,
                ..crate::font::UI
            };
            let title_color = if gone {
                self.theme.muted().scale_alpha(0.4)
            } else {
                self.theme.extended().background.base.text
            };
            // Fill: a long title wraps instead of pushing the buttons out.
            head = head.push(
                text(section.title.as_str())
                    .size(size)
                    .font(strong)
                    .color(title_color)
                    .width(Length::Fill),
            );
            if gone {
                head = head.push(text("deleted").size(TEXT_SM).color(self.theme.error()));
            }
            // A section for a ticket of its own: where it is filed on
            // approval, and the ticket it became.
            let mut filing = None;
            if section.level == 2
                && let Some(heading) =
                    proposal_heading(&section.title, |name| client.repos.contains_key(name))
            {
                let mut badge = format!("to {}", heading.repo);
                if let Some(label) = &heading.label {
                    badge.push_str(&format!(" #{label}"));
                }
                let mut line = row![text(badge).size(TEXT_SM).color(self.theme.accent())]
                    .spacing(SP_2)
                    .align_y(Alignment::Center);
                if let Some(filed) = doc
                    .filed
                    .iter()
                    .find(|f| f.repo == heading.repo && f.title == heading.title)
                {
                    line = line.push(self.ref_chip(
                        popout,
                        client,
                        state,
                        &doc.repo,
                        &filed.repo,
                        filed.number,
                    ));
                }
                filing = Some(line);
            }
            if review {
                head = head.push(tip(
                    icon_button(EDIT, None).on_press(Message::Ticket(TicketMsg::Annotate(at))),
                    "Annotate".to_owned(),
                ));
                let own = draft.is_some_and(|draft| draft.deleted.contains(&at));
                let (glyph_of, words) = if own {
                    (glyph::RERUN, "Restore")
                } else {
                    (TRASH, "Delete with its subsections")
                };
                let mut delete = icon_button(glyph_of, None);
                if !under_gone {
                    delete = delete.on_press(Message::Ticket(TicketMsg::Delete(at)));
                }
                head = head.push(tip(delete, words.to_owned()));
            }
            let ci = popout.ci.clone();
            let body = markdown::view(content.items(), self.markdown_settings())
                .map(move |uri| Message::Ci(ci.clone(), CiMsg::OpenLink(uri)));
            // Markdown's paragraphs take the surrounding text colour.
            let faded = self.theme.extended().background.base.text.scale_alpha(0.4);
            let body = container(body).style(move |_: &Theme| container::Style {
                text_color: gone.then_some(faded),
                ..container::Style::default()
            });
            let mut card = column![head].spacing(SP_1);
            if let Some(filing) = filing {
                card = card.push(filing);
            }
            card = card.push(body);
            if review
                && let Some(draft) = draft
                && draft.open_notes.contains(&at)
                && !gone
            {
                let note = draft.notes.get(&at).unwrap_or(&popout.blank);
                card = card.push(
                    text_editor(note)
                        .placeholder("What should change here")
                        .height(OVERALL_HEIGHT)
                        .on_action(move |action| Message::Ticket(TicketMsg::Note(at, action))),
                );
            }
            let indent = SP_4 * f32::from(section.level.saturating_sub(1));
            part = part.push(row![Space::new().width(indent), card.width(Length::Fill)]);
        }
        if shown.sections.is_empty() {
            part = part.push(text("No plan yet.").size(TEXT_SM).color(muted));
        }
        part.into()
    }

    /// A chip of a referenced ticket. One of `own_repo` shows its state's
    /// glyph, number and title; one of another repository its `#repo-n`.
    /// A press opens it, on the Board of its repository; a repository the
    /// CI does not know gives plain text.
    fn ref_chip<'a>(
        &self,
        popout: &TicketPopout,
        client: &CiClient,
        state: &'a PlansState,
        own_repo: &str,
        repo: &str,
        number: u64,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let (words, glyph_of, view) = if repo == own_repo {
            let linked = state.doc(number);
            let words = match linked {
                Some(doc) => format!("#{number} {}", shortened(&doc.title)),
                None => format!("#{number}"),
            };
            let glyph_of = linked.map_or(glyph::DOT, |doc| state_glyph(doc.state));
            (words, glyph_of, Some(popout.view))
        } else {
            (
                format!("#{repo}-{number}"),
                glyph::DOT,
                client.plans_view_of(repo),
            )
        };
        let content = row![icon(glyph_of, TEXT_SM, muted), text(words).size(TEXT_SM)]
            .spacing(SP_1)
            .align_y(Alignment::Center);
        let Some(view) = view else {
            return container(content).padding([2.0, SP_2]).into();
        };
        let ci = popout.ci.clone();
        button(content)
            .padding([2.0, SP_2])
            .style(style::secondary_button(&Tokens::of(&self.theme)))
            .on_press_with(move || {
                Message::Ticket(TicketMsg::Open {
                    ci: ci.clone(),
                    view,
                    number,
                })
            })
            .into()
    }

    /// The side column: state, source, `Runs on`, links both ways (edited
    /// in its text), the tickets its plan's sections became, pre-approval,
    /// what it includes, its sessions and who holds it.
    fn side_column<'a>(
        &'a self,
        popout: &'a TicketPopout,
        client: &'a CiClient,
        state: &'a PlansState,
        doc: &'a DocView,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let now = now();
        let block =
            |label: &'a str, content: Element<'a, Message, Theme>| -> Element<'a, Message, Theme> {
                column![text(label).size(TEXT_SM).color(muted), content]
                    .spacing(SP_1)
                    .into()
            };
        let line =
            |words: String| -> Element<'a, Message, Theme> { text(words).size(TEXT_SM).into() };
        let mut side = column![].spacing(SP_3).padding([0.0, SP_2]);
        side = side.push(block(
            "State",
            line(format!(
                "{} \u{b7} {}",
                doc.state.name().replace('_', " "),
                ago(doc.changed, now)
            )),
        ));
        side = side.push(block(
            "From",
            line(format!("{} \u{b7} {}", doc.source, ago(doc.created, now))),
        ));
        let runs_on = self
            .runs_on_line(&popout.ci, popout.view, state, doc)
            .unwrap_or_else(|| text("any runner").size(TEXT_SM).color(muted).into());
        side = side.push(block("Runs on", runs_on));
        if doc.kind != DocKind::Ticket {
            return side.into();
        }
        let incoming = linked_from(doc.number, state.docs());
        let chip =
            |repo: &str, number: u64| self.ref_chip(popout, client, state, &doc.repo, repo, number);
        let outgoing = |kind: LinkKind| -> Element<'a, Message, Theme> {
            let mut chips = column![].spacing(SP_1);
            for link in doc.links.iter().filter(|link| link.kind == kind) {
                chips = chips.push(chip(link.repo.as_deref().unwrap_or(&doc.repo), link.number));
            }
            if kind == LinkKind::Related {
                for (_, number) in incoming.iter().filter(|(k, _)| *k == LinkKind::Related) {
                    chips = chips.push(chip(&doc.repo, *number));
                }
            }
            chips.into()
        };
        side = side.push(block("Depends on", outgoing(LinkKind::DependsOn)));
        side = side.push(block("Related", outgoing(LinkKind::Related)));
        let blocks: Vec<u64> = incoming
            .iter()
            .filter(|(kind, _)| *kind == LinkKind::DependsOn)
            .map(|(_, number)| *number)
            .collect();
        if !blocks.is_empty() {
            let mut chips = column![].spacing(SP_1);
            for number in blocks {
                chips = chips.push(chip(&doc.repo, number));
            }
            side = side.push(block("Blocks", chips.into()));
        }
        if !doc.filed.is_empty() {
            let mut chips = column![].spacing(SP_1);
            for filed in &doc.filed {
                chips = chips.push(chip(&filed.repo, filed.number));
            }
            side = side.push(block("Filed", chips.into()));
        }
        let review: Element<'a, Message, Theme> = if matches!(
            doc.state,
            DocState::Open | DocState::Ready | DocState::Planning
        ) {
            let words = if doc.auto_approve {
                "pre-approved"
            } else {
                "pre-approve"
            };
            let mut pill = self.ticket_pill(words, doc.auto_approve);
            if !state.acting {
                pill = pill.on_press(Message::Ticket(TicketMsg::PreApprove(!doc.auto_approve)));
            }
            tip(
                pill,
                "A pre-approved plan is approved as it arrives".to_owned(),
            )
        } else if doc.auto_approve {
            line("pre-approved".to_owned())
        } else {
            line("reviewed".to_owned())
        };
        side = side.push(block("Plan review", review));
        if !doc.merged.is_empty() {
            let mut chips = column![].spacing(SP_1);
            for number in &doc.merged {
                chips = chips.push(chip(&doc.repo, *number));
            }
            side = side.push(block("Includes", chips.into()));
        }
        if let Some(into) = doc.into {
            side = side.push(block("Merged into", chip(&doc.repo, into)));
        }
        let mut sessions: Vec<_> = client
            .sessions()
            .filter(|s| s.repo == doc.repo && s.doc == Some(doc.number))
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.id));
        sessions.truncate(SESSIONS_SHOWN);
        if !sessions.is_empty() {
            let mut rows = column![].spacing(SP_2);
            for session in sessions {
                let role = match session.kind {
                    SessionKind::Plan => AgentRole::Plan,
                    SessionKind::Master => AgentRole::Master,
                };
                let mut entry = column![
                    row![
                        icon(role_glyph(role), TEXT_SM, muted),
                        text(format!(
                            "{} on {} \u{b7} {} \u{b7} {}",
                            session.kind.name(),
                            session.runner,
                            session.state.name(),
                            ago(session.started, now)
                        ))
                        .size(TEXT_SM),
                    ]
                    .spacing(SP_1)
                    .align_y(Alignment::Center)
                ]
                .spacing(SP_1);
                if session.state.is_live() {
                    let mut open = self.ticket_pill("Open", false);
                    if let (Some(key), Some(terminal)) =
                        (client.runner_key(&session.runner), session.terminal)
                    {
                        open = open.on_press_with(move || {
                            Message::RevealTerminal(key.clone(), TerminalId(terminal))
                        });
                    }
                    let mut stop = self.ticket_pill("Stop", false);
                    if !state.acting {
                        let (ci, view) = (popout.ci.clone(), popout.view);
                        let id = session.id;
                        stop = stop.on_press_with(move || {
                            Message::Ci(
                                ci.clone(),
                                CiMsg::DocAct {
                                    view,
                                    request: CiRequest::CloseSession { id },
                                },
                            )
                        });
                    }
                    entry = entry.push(row![open, stop].spacing(SP_1));
                }
                rows = rows.push(entry);
            }
            side = side.push(block("Sessions", rows.into()));
        }
        if let Some(claim) = client.claim_on(&ClaimSubject::Ticket {
            repo: doc.repo.clone(),
            number: doc.number,
        }) {
            let mut holder = button(text(claim.holder_text()).size(TEXT_SM))
                .padding(0)
                .style(super::link_style);
            if let Some((key, terminal)) = client.holder_target(claim) {
                holder =
                    holder.on_press_with(move || Message::RevealTerminal(key.clone(), terminal));
            }
            side = side.push(block("Claimed by", holder.into()));
        }
        side.into()
    }
}
