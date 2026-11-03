//! The Board of a project no CI builds: what it is and where it stands,
//! cards of work to choose, and a preview of what `Onboard` files.
//!
//! Showing the Board asks the local runner for the facts about the project
//! (the skills installed there, its listing, its work checkout or its
//! forge) and hands them to the first CI, whose `[advice]` model answers
//! with a summary, the onboarding's settings and cards: pieces of work,
//! each the tickets it files, which may depend on tickets of other cards.
//! The owner picks cards and switches a ticket between running through
//! (its plan is approved as it arrives) and waiting for review. `Onboard`
//! files the request already approved together with the chosen tickets in
//! one step; the CI joins the repository, and the tickets go through
//! planning and the queue in the order their dependencies allow.

use std::collections::HashMap;

use iced::widget::{button, column, container, mouse_area, row, rule, scrollable, text};
use iced::{Alignment, Border, Element, Length, Task};
use zeughaus_link::{
    AdviceView, Card, CiReply, CiRequest, NewTicket, OnboardSettings, OnboardSpec, ProjectsRequest,
};
use zeughaus_theme::Theme;

use super::{ago, now};
use crate::app::App;
use crate::message::{Message, OnboardMsg};
use crate::notices::Severity;
use crate::style::{self, Tokens};

/// The widest the Board's column grows.
const MAX_WIDTH: f32 = 1040.0;
/// The width of a card.
const CARD_WIDTH: f32 = 320.0;
/// Text of a line, and of a caption.
const ROW_TEXT: f32 = 13.0;
const CAPTION_TEXT: f32 = 12.0;

/// The Boards of projects no CI builds, by origin.
pub(in crate::app) type Drafts = HashMap<String, Draft>;

/// What the Board of one project no CI builds holds.
#[derive(Default)]
pub(in crate::app) struct Draft {
    /// The facts or the advice are on their way.
    asking: bool,
    /// Ask the CI past its kept answer.
    refresh: bool,
    advice: Option<AdviceView>,
    /// The advice's cards as the owner has chosen and switched them.
    cards: Vec<Card>,
    /// Why no advice arrived.
    error: Option<String>,
    filing: bool,
    /// Why the last onboarding was refused.
    refused: Option<String>,
}

/// One ticket `Onboard` files, as the preview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    card: usize,
    ticket: usize,
    /// Places among the filed tickets of the ones it waits for.
    depends_on: Vec<usize>,
    /// Titles of the tickets it would wait for whose card is not chosen.
    dropped: Vec<String>,
}

/// The tickets of the chosen cards in filing order, their dependencies
/// as places in that order; a dependency on a ticket of a card that is
/// not chosen is dropped and named.
fn planned(cards: &[Card]) -> Vec<Planned> {
    let mut places: HashMap<&str, usize> = HashMap::new();
    let mut order = Vec::new();
    for (at, card) in cards.iter().enumerate().filter(|(_, card)| card.chosen) {
        for (ticket_at, ticket) in card.tickets.iter().enumerate() {
            places.insert(ticket.key.as_str(), order.len());
            order.push((at, ticket_at));
        }
    }
    let titles: HashMap<&str, &str> = cards
        .iter()
        .flat_map(|card| &card.tickets)
        .map(|ticket| (ticket.key.as_str(), ticket.title.as_str()))
        .collect();
    order
        .into_iter()
        .map(|(card, ticket_at)| {
            let ticket = &cards[card].tickets[ticket_at];
            let mut depends_on = Vec::new();
            let mut dropped = Vec::new();
            for on in &ticket.depends_on {
                match places.get(on.as_str()) {
                    Some(&place) => depends_on.push(place),
                    None => {
                        dropped.push(titles.get(on.as_str()).unwrap_or(&on.as_str()).to_string())
                    }
                }
            }
            Planned {
                card,
                ticket: ticket_at,
                depends_on,
                dropped,
            }
        })
        .collect()
}

/// The `Onboard` request: the onboarding with the chosen cards' tickets.
fn onboarding(
    origin: &str,
    summary: &str,
    settings: &OnboardSettings,
    cards: &[Card],
) -> CiRequest {
    let planned = planned(cards);
    let mut body = format!("Onboarded from the editor: {origin}.\n");
    if !summary.trim().is_empty() {
        body.push_str(&format!("\n{}\n", summary.trim()));
    }
    let chosen: Vec<&Card> = cards.iter().filter(|card| card.chosen).collect();
    if !chosen.is_empty() {
        body.push_str("\nCards:\n");
        for card in &chosen {
            body.push_str(&format!("- {} ({})\n", card.title, card.kind.name()));
        }
    }
    let tickets = planned
        .iter()
        .map(|planned| {
            let card = &cards[planned.card];
            let ticket = &card.tickets[planned.ticket];
            let mut body = ticket.proposal.trim().to_owned();
            if !ticket.skills.is_empty() {
                let skills: Vec<String> = ticket
                    .skills
                    .iter()
                    .map(|s| format!("skill://{s}"))
                    .collect();
                body.push_str(&format!("\n\nSkills to read first: {}.", skills.join(", ")));
            }
            body.push_str(&format!(
                "\n\nFrom the onboarding of {origin}, card \"{}\".\n",
                card.title
            ));
            NewTicket {
                title: ticket.title.clone(),
                body,
                depends_on: planned.depends_on.clone(),
                auto_approve: ticket.auto,
            }
        })
        .collect();
    CiRequest::Onboard {
        repo: settings.name.clone(),
        title: format!("Onboard {}", settings.name),
        body,
        spec: OnboardSpec {
            url: format!("https://{origin}"),
            default_branch: settings.default_branch.clone(),
            grants: settings.grants.clone(),
            prerequisites: settings.prerequisites.clone(),
        },
        tickets,
    }
}

/// The settings without advice: the origin's last segment as the name,
/// `branch` as the default branch.
fn plain_settings(origin: &str, branch: &str) -> OnboardSettings {
    OnboardSettings {
        name: origin
            .rsplit('/')
            .next()
            .unwrap_or(origin)
            .to_lowercase()
            .replace(['_', '.'], "-"),
        default_branch: branch.to_owned(),
        ..OnboardSettings::default()
    }
}

impl App {
    /// The Board of `origin` came to the front: asks for its advice the
    /// first time, once a CI is connected.
    pub(in crate::app) fn ensure_advice(&mut self, origin: &str) -> Task<Message> {
        if self.ci.is_empty()
            || self.page.onboard.contains_key(origin)
            || self.ci_of(origin).is_some()
        {
            return Task::none();
        }
        self.ask_advice(origin, false)
    }

    /// Asks the local runner for the facts about `origin`; the CI is asked
    /// with them once they arrive, or without a runner at once.
    fn ask_advice(&mut self, origin: &str, refresh: bool) -> Task<Message> {
        let draft = self.page.onboard.entry(origin.to_owned()).or_default();
        if draft.asking {
            return Task::none();
        }
        draft.asking = true;
        draft.refresh = refresh;
        draft.error = None;
        let endpoint = self
            .local_runner
            .as_ref()
            .and_then(|key| self.runtime.links.get(key))
            .map(|link| link.endpoint.clone());
        let origin = origin.to_owned();
        match endpoint {
            Some(endpoint) => Task::perform(
                crate::feed::projects(
                    endpoint,
                    ProjectsRequest {
                        refresh: false,
                        facts: Some(origin.clone()),
                    },
                ),
                move |result| Message::Onboard(origin.clone(), OnboardMsg::Facts(result)),
            ),
            None => self.ask_ci_advice(
                &origin,
                format!("# {origin}\n\n- no runner on the editor's machine to look at it\n"),
            ),
        }
    }

    /// Hands `facts` to the first CI.
    fn ask_ci_advice(&mut self, origin: &str, facts: String) -> Task<Message> {
        let refresh = self
            .page
            .onboard
            .get(origin)
            .is_some_and(|draft| draft.refresh);
        let Some(client) = self.ci.values().next() else {
            if let Some(draft) = self.page.onboard.get_mut(origin) {
                draft.asking = false;
                draft.error = Some("no CI is connected".to_owned());
            }
            return Task::none();
        };
        let request = CiRequest::Advise {
            origin: origin.to_owned(),
            facts,
            refresh,
        };
        let (endpoint, origin) = (client.endpoint.clone(), origin.to_owned());
        Task::perform(
            async move {
                match crate::feed::ci(endpoint, request).await? {
                    CiReply::Advice { advice } => Ok(advice),
                    _ => Err(super::UNEXPECTED.to_owned()),
                }
            },
            move |result| Message::Onboard(origin.clone(), OnboardMsg::Advice(result)),
        )
    }

    /// The default branch the local runner lists `origin` with.
    fn default_branch_of(&self, origin: &str) -> String {
        self.listed_projects()
            .into_iter()
            .find(|p| p.origin == origin)
            .map_or_else(|| "main".to_owned(), |p| p.default_branch)
    }

    /// The settings `Onboard` files for `origin`: the advice's, else plain
    /// ones.
    fn settings_of(&self, origin: &str, draft: &Draft) -> OnboardSettings {
        match &draft.advice {
            Some(advice) if !advice.onboard.name.is_empty() => advice.onboard.clone(),
            _ => plain_settings(origin, &self.default_branch_of(origin)),
        }
    }

    /// What a Board of a project no CI builds is told.
    pub(in crate::app) fn apply_onboard(
        &mut self,
        origin: String,
        message: OnboardMsg,
    ) -> Task<Message> {
        match message {
            OnboardMsg::Facts(result) => {
                let facts = match result {
                    Ok(reply) => reply.facts.unwrap_or_default(),
                    Err(e) => format!("# {origin}\n\n- the runner could not look at it: {e}\n"),
                };
                return self.ask_ci_advice(&origin, facts);
            }
            OnboardMsg::Refresh => return self.ask_advice(&origin, true),
            OnboardMsg::Onboard => return self.file_onboarding(&origin),
            _ => {}
        }
        let Some(draft) = self.page.onboard.get_mut(&origin) else {
            return Task::none();
        };
        match message {
            OnboardMsg::Advice(result) => {
                draft.asking = false;
                match result {
                    Ok(advice) => {
                        draft.error = None;
                        draft.cards = advice.cards.clone();
                        draft.advice = Some(*advice);
                    }
                    Err(e) => draft.error = Some(e),
                }
            }
            OnboardMsg::Card(at) => {
                if let Some(card) = draft.cards.get_mut(at) {
                    card.chosen = !card.chosen;
                }
            }
            OnboardMsg::Mode(card, ticket) => {
                if let Some(ticket) = draft
                    .cards
                    .get_mut(card)
                    .and_then(|card| card.tickets.get_mut(ticket))
                {
                    ticket.auto = !ticket.auto;
                }
            }
            OnboardMsg::Onboarded(result) => {
                draft.filing = false;
                match result {
                    Ok((request, tickets)) => {
                        // The CI lists the pending repository from now on,
                        // and its Board takes this one's place.
                        self.page.onboard.remove(&origin);
                        let body = match tickets.len() {
                            0 => format!("Request #{request} is approved; the CI joins it now."),
                            n => format!(
                                "Request #{request} is approved with {n} ticket{}; they start once the CI has joined it.",
                                if n == 1 { "" } else { "s" }
                            ),
                        };
                        self.notices.raise(
                            None,
                            Severity::Info,
                            format!("Onboarding {origin}"),
                            body,
                            false,
                            None,
                            std::time::SystemTime::now(),
                        );
                        return self.poll_ci();
                    }
                    Err(e) => draft.refused = Some(e),
                }
            }
            OnboardMsg::Facts(_) | OnboardMsg::Refresh | OnboardMsg::Onboard => {}
        }
        Task::none()
    }

    /// Files the onboarding with the chosen cards' tickets.
    fn file_onboarding(&mut self, origin: &str) -> Task<Message> {
        let Some(client) = self.ci.values().next() else {
            return Task::none();
        };
        let endpoint = client.endpoint.clone();
        let Some(draft) = self.page.onboard.get(origin) else {
            return Task::none();
        };
        let settings = self.settings_of(origin, draft);
        let summary = draft
            .advice
            .as_ref()
            .map(|a| a.summary.clone())
            .unwrap_or_default();
        let request = onboarding(origin, &summary, &settings, &draft.cards);
        if let Some(draft) = self.page.onboard.get_mut(origin) {
            draft.filing = true;
            draft.refused = None;
        }
        let origin = origin.to_owned();
        Task::perform(
            async move {
                match crate::feed::ci(endpoint, request).await? {
                    CiReply::Onboarded { request, tickets } => Ok((request, tickets)),
                    _ => Err(super::UNEXPECTED.to_owned()),
                }
            },
            move |result| Message::Onboard(origin.clone(), OnboardMsg::Onboarded(result)),
        )
    }

    /// The Board of a project no CI builds.
    pub(in crate::app) fn unbuilt_board<'a>(
        &'a self,
        t: &Tokens,
        origin: &'a str,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let send = move |message: OnboardMsg| Message::Onboard(origin.to_owned(), message);
        let listed = self
            .listed_projects()
            .into_iter()
            .find(|p| p.origin == origin);
        let draft = self.page.onboard.get(origin);
        let advice = draft.and_then(|d| d.advice.as_ref());

        // What it is and where it stands.
        let mut facts = vec!["not built by a CI".to_owned()];
        if let Some(listed) = &listed {
            facts.push(format!("default branch {}", listed.default_branch));
            if listed.checkout {
                facts.push(format!("checked out in ~/work/{origin}"));
            }
        }
        let mut head = row![
            column![
                text(origin).size(18),
                text(facts.join(" \u{b7} ")).size(CAPTION_TEXT).color(muted),
            ]
            .spacing(4)
            .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if let Some(advice) = advice.filter(|a| !a.model.is_empty()) {
            head = head.push(
                text(format!(
                    "advice by {}, {}",
                    advice.model,
                    ago(advice.created, now())
                ))
                .size(CAPTION_TEXT)
                .color(muted),
            );
        }
        if !self.ci.is_empty() {
            let asking = draft.is_some_and(|d| d.asking);
            head = head.push(
                button(
                    text(if asking {
                        "Asking\u{2026}"
                    } else {
                        "Ask again"
                    })
                    .size(CAPTION_TEXT),
                )
                .padding([4, 12])
                .style(style::secondary_button(t))
                .on_press_maybe((!asking).then(|| send(OnboardMsg::Refresh))),
            );
        }
        let mut body = column![head].spacing(16);
        if self.ci.is_empty() {
            body = body.push(text("No CI configured").size(ROW_TEXT).color(muted));
            return page(body);
        }
        let Some(draft) = draft else {
            body = body.push(
                button(text("Ask the CI about it").size(CAPTION_TEXT))
                    .padding([4, 14])
                    .style(style::pill_button(t))
                    .on_press(send(OnboardMsg::Refresh)),
            );
            return page(body);
        };
        if draft.asking && advice.is_none() {
            body = body.push(
                text("Looking at the project\u{2026}")
                    .size(ROW_TEXT)
                    .color(muted),
            );
        }
        if let Some(error) = &draft.error {
            body = body.push(
                text(error.as_str())
                    .size(ROW_TEXT)
                    .color(self.theme.error()),
            );
        }
        if let Some(advice) = advice {
            if !advice.summary.is_empty() {
                body = body.push(text(advice.summary.as_str()).size(ROW_TEXT));
            }
            if !advice.status.is_empty() {
                let mut lines = column![].spacing(2);
                for line in &advice.status {
                    lines = lines.push(
                        text(format!("\u{2022} {line}"))
                            .size(CAPTION_TEXT)
                            .color(muted),
                    );
                }
                body = body.push(lines);
            }
            if let Some(error) = &advice.error {
                body = body.push(
                    text(format!("No advice: {error}"))
                        .size(CAPTION_TEXT)
                        .color(muted),
                );
            }
        }
        if !draft.cards.is_empty() {
            body = body.push(caption("Choose the work", muted));
            let mut cards = row![].spacing(12);
            for (at, card) in draft.cards.iter().enumerate() {
                cards = cards.push(self.card_view(t, origin, at, card, &draft.cards));
            }
            body = body.push(cards.wrap().vertical_spacing(12));
        }
        body = body
            .push(rule::horizontal(1))
            .push(caption("Onboard files", muted))
            .push(self.preview(t, origin, draft));
        page(body)
    }

    /// One card: its kind, title and what it brings, and its tickets, each
    /// with its mode, skills and what it waits for. A press chooses or
    /// drops it; a press on a ticket's mode switches it.
    fn card_view<'a>(
        &'a self,
        t: &Tokens,
        origin: &'a str,
        at: usize,
        card: &'a Card,
        cards: &'a [Card],
    ) -> Element<'a, Message, Theme> {
        let send = move |message: OnboardMsg| Message::Onboard(origin.to_owned(), message);
        let (muted, accent, surface) = (self.theme.muted(), self.theme.accent(), t.surface);
        let chosen = card.chosen;
        let mut lines = column![
            row![
                chip(t, card.kind.name(), chosen),
                text(card.title.as_str()).size(ROW_TEXT).width(Length::Fill),
                text(if chosen { "\u{2713}" } else { "" })
                    .size(ROW_TEXT)
                    .color(accent),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(card.body.as_str()).size(CAPTION_TEXT).color(muted),
        ]
        .spacing(6);
        for (ticket_at, ticket) in card.tickets.iter().enumerate() {
            let mode = button(
                text(if ticket.auto {
                    "runs through"
                } else {
                    "review"
                })
                .size(11),
            )
            .padding([0, 6]);
            let mode = if ticket.auto {
                mode.style(style::pill_button(t))
            } else {
                mode.style(style::secondary_button(t))
            }
            .on_press(send(OnboardMsg::Mode(at, ticket_at)));
            let mut line = row![mode, text(ticket.title.as_str()).size(CAPTION_TEXT)]
                .spacing(6)
                .align_y(Alignment::Center);
            for skill in &ticket.skills {
                line = line.push(chip(t, skill, false));
            }
            let mut ticket_lines = column![line.wrap().vertical_spacing(4)].spacing(2);
            let waits: Vec<&str> = ticket
                .depends_on
                .iter()
                .filter_map(|on| {
                    cards
                        .iter()
                        .flat_map(|c| &c.tickets)
                        .find(|other| &other.key == on)
                        .map(|other| other.title.as_str())
                })
                .collect();
            if !waits.is_empty() {
                ticket_lines = ticket_lines.push(
                    text(format!("after {}", waits.join(", ")))
                        .size(11)
                        .color(muted),
                );
            }
            lines = lines.push(ticket_lines);
        }
        let card = container(lines)
            .width(CARD_WIDTH)
            .padding(10)
            .style(move |_: &Theme| container::Style {
                background: Some(surface.into()),
                border: Border {
                    color: if chosen {
                        accent
                    } else {
                        muted.scale_alpha(0.3)
                    },
                    width: if chosen { 2.0 } else { 1.0 },
                    radius: 6.0.into(),
                },
                ..container::Style::default()
            });
        mouse_area(card)
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press(send(OnboardMsg::Card(at)))
            .into()
    }

    /// What `Onboard` files: the request with its settings, then each
    /// chosen ticket in filing order with its mode and what it waits for;
    /// and the button.
    fn preview<'a>(
        &'a self,
        t: &Tokens,
        origin: &'a str,
        draft: &'a Draft,
    ) -> Element<'a, Message, Theme> {
        let send = move |message: OnboardMsg| Message::Onboard(origin.to_owned(), message);
        let muted = self.theme.muted();
        let settings = self.settings_of(origin, draft);
        let mut request = format!(
            "1. Onboarding request for `{}`, branch {}, approved",
            settings.name, settings.default_branch
        );
        for (secret, events) in &settings.grants {
            request.push_str(&format!("; secret {secret} for {}", events.join(", ")));
        }
        let mut list = column![text(request).size(ROW_TEXT)].spacing(4);
        for line in &settings.prerequisites {
            list = list.push(
                text(format!("   host prerequisite: {line}"))
                    .size(CAPTION_TEXT)
                    .color(muted),
            );
        }
        let planned = planned(&draft.cards);
        for (place, planned) in planned.iter().enumerate() {
            let ticket = &draft.cards[planned.card].tickets[planned.ticket];
            let mut line = format!(
                "{}. {} \u{2014} {}",
                place + 2,
                ticket.title,
                if ticket.auto {
                    "runs through"
                } else {
                    "plan for review"
                }
            );
            if !planned.depends_on.is_empty() {
                let after: Vec<String> = planned
                    .depends_on
                    .iter()
                    .map(|p| format!("{}", p + 2))
                    .collect();
                line.push_str(&format!(", after {}", after.join(", ")));
            }
            list = list.push(text(line).size(ROW_TEXT));
            if !planned.dropped.is_empty() {
                list = list.push(
                    text(format!(
                        "   no longer waits for {} (its card is not chosen)",
                        planned.dropped.join(", ")
                    ))
                    .size(CAPTION_TEXT)
                    .color(muted),
                );
            }
        }
        let refusal = draft.advice.as_ref().and_then(|a| a.refusal.as_ref());
        let words = match (draft.filing, planned.len()) {
            (true, _) => "Onboarding\u{2026}".to_owned(),
            (false, 0) => "Onboard".to_owned(),
            (false, 1) => "Onboard with 1 ticket".to_owned(),
            (false, n) => format!("Onboard with {n} tickets"),
        };
        let mut footer = row![
            button(text(words).size(CAPTION_TEXT))
                .padding([5, 16])
                .style(style::pill_button(t))
                .on_press_maybe(
                    (!draft.filing && !draft.asking && refusal.is_none())
                        .then(|| send(OnboardMsg::Onboard))
                ),
            text(
                "The CI adds its bot and push hook and builds the default branch; then the \
                 tickets are planned in the order they wait for each other."
            )
            .size(CAPTION_TEXT)
            .color(muted)
            .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if let Some(refused) = &draft.refused {
            footer = footer.push(
                text(refused.as_str())
                    .size(CAPTION_TEXT)
                    .color(self.theme.error()),
            );
        }
        let mut block = column![list].spacing(12);
        if let Some(refusal) = refusal {
            block = block.push(
                text(format!("Cannot be onboarded as it is: {refusal}"))
                    .size(ROW_TEXT)
                    .color(self.theme.readable(self.theme.hint())),
            );
        }
        block.push(footer).into()
    }
}

/// The Board's column, scrollable, as wide as reads well.
fn page<'a>(body: iced::widget::Column<'a, Message, Theme>) -> Element<'a, Message, Theme> {
    scrollable(
        container(body.max_width(MAX_WIDTH).padding(24))
            .width(Length::Fill)
            .center_x(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}

/// A caption over a part of the Board.
fn caption<'a>(words: &'a str, muted: iced::Color) -> Element<'a, Message, Theme> {
    text(words.to_uppercase())
        .size(CAPTION_TEXT)
        .color(muted)
        .into()
}

/// A small label.
fn chip<'a>(t: &Tokens, words: &'a str, accent: bool) -> Element<'a, Message, Theme> {
    container(text(words).size(11))
        .padding([0, 6])
        .style(style::badge(t, accent))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::{CardKind, CardTicket};

    fn ticket(key: &str, depends_on: &[&str], auto: bool) -> CardTicket {
        CardTicket {
            key: key.to_owned(),
            title: format!("Do {key}"),
            proposal: format!("Do {key} well."),
            skills: if key == "jobs" {
                vec!["zeughaus-ci".to_owned()]
            } else {
                Vec::new()
            },
            depends_on: depends_on.iter().map(|on| (*on).to_owned()).collect(),
            auto,
        }
    }

    fn card(title: &str, chosen: bool, tickets: Vec<CardTicket>) -> Card {
        Card {
            kind: CardKind::Build,
            title: title.to_owned(),
            body: String::new(),
            chosen,
            tickets,
        }
    }

    #[test]
    fn the_chosen_cards_file_their_tickets_with_dependencies_by_place() {
        let cards = vec![
            card("Docs", false, vec![ticket("docs", &[], true)]),
            card(
                "Deploy",
                true,
                vec![ticket("deploy", &["jobs", "docs"], false)],
            ),
            card("Jobs", true, vec![ticket("jobs", &[], true)]),
        ];
        assert_eq!(
            planned(&cards),
            [
                Planned {
                    card: 1,
                    ticket: 0,
                    depends_on: vec![1],
                    dropped: vec!["Do docs".to_owned()],
                },
                Planned {
                    card: 2,
                    ticket: 0,
                    depends_on: Vec::new(),
                    dropped: Vec::new(),
                },
            ]
        );
        let settings = plain_settings("git.example.org/o/My_Site", "trunk");
        let CiRequest::Onboard {
            repo,
            spec,
            tickets,
            ..
        } = onboarding("git.example.org/o/My_Site", "A site.", &settings, &cards)
        else {
            panic!("an onboarding");
        };
        assert_eq!(repo, "my-site");
        assert_eq!(
            (spec.url.as_str(), spec.default_branch.as_str()),
            ("https://git.example.org/o/My_Site", "trunk")
        );
        assert_eq!(
            tickets,
            [
                NewTicket {
                    title: "Do deploy".to_owned(),
                    body: "Do deploy well.\n\nFrom the onboarding of git.example.org/o/My_Site, \
                           card \"Deploy\".\n"
                        .to_owned(),
                    depends_on: vec![1],
                    auto_approve: false,
                },
                NewTicket {
                    title: "Do jobs".to_owned(),
                    body: "Do jobs well.\n\nSkills to read first: skill://zeughaus-ci.\n\n\
                           From the onboarding of git.example.org/o/My_Site, card \"Jobs\".\n"
                        .to_owned(),
                    depends_on: Vec::new(),
                    auto_approve: true,
                },
            ]
        );
    }
}
