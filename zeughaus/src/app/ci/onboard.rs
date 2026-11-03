//! The Board of a project no CI builds: the CI's small plan document for
//! it, its `## ` sections as cards to choose, and what `Onboard` files.
//!
//! Showing the Board asks the local runner for the facts about the project
//! (the skills installed there, its listing, its work checkout or its
//! forge) and hands them to the first CI, whose `[advice]` model writes the
//! plan: a `# ` title and the text under it, then one `## ` section per
//! piece of work. The owner picks sections and switches each between
//! running through (its plan is approved as it arrives) and waiting for
//! review. `Onboard` files the request already approved together with one
//! ticket per chosen section in one step, each waiting for the one before
//! it; the CI joins the repository and the tickets follow in that order.

use std::collections::HashMap;

use iced::widget::{button, column, container, markdown, mouse_area, row, rule, text};
use iced::{Alignment, Border, Element, Length, Task};
use iced_tabs::scroll;
use zeughaus_link::{
    AdviceView, Card, CiReply, CiRequest, NewTicket, OnboardSpec, ProjectsRequest,
};
use zeughaus_theme::Theme;

use super::{ago, now};
use crate::app::App;
use crate::message::{CiMsg, Message, OnboardMsg};
use crate::notices::Severity;
use crate::style::{self, Tokens};

/// The widest the Board's column grows.
const MAX_WIDTH: f32 = 1040.0;
/// The width of a card: two side by side in the widest column.
const CARD_WIDTH: f32 = 490.0;
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
    /// The advice's cards as the owner has chosen them.
    cards: Vec<Card>,
    /// Per card: its ticket runs through instead of waiting for review.
    through: Vec<bool>,
    /// The summary and each card's text, parsed once.
    summary: Option<markdown::Content>,
    bodies: Vec<markdown::Content>,
    /// Why no advice arrived.
    error: Option<String>,
    filing: bool,
    /// Why the last onboarding was refused.
    refused: Option<String>,
}

/// The repository's name in `ci.toml` for `origin`: its last segment in
/// lowercase, `_` and `.` made dashes.
fn repo_name(origin: &str) -> String {
    origin
        .rsplit('/')
        .next()
        .unwrap_or(origin)
        .to_lowercase()
        .replace(['_', '.'], "-")
}

/// The `Onboard` request: the onboarding of `origin` with one ticket per
/// chosen card in the plan's order, each waiting for the one before.
fn onboarding(
    origin: &str,
    branch: &str,
    advice: Option<&AdviceView>,
    cards: &[Card],
    through: &[bool],
) -> CiRequest {
    let name = repo_name(origin);
    let mut body = format!("Onboarded from the editor: {origin}.\n");
    if let Some(advice) = advice {
        if !advice.title.is_empty() {
            body.push_str(&format!("\n# {}\n", advice.title));
        }
        if !advice.summary.is_empty() {
            body.push_str(&format!("\n{}\n", advice.summary));
        }
    }
    let mut tickets: Vec<NewTicket> = Vec::new();
    for (at, card) in cards.iter().enumerate().filter(|(_, card)| card.chosen) {
        let depends_on = tickets.len().checked_sub(1).into_iter().collect();
        tickets.push(NewTicket {
            title: card.title.clone(),
            body: format!(
                "{}\n\nFrom the onboarding plan of {origin}.\n",
                card.body.trim()
            ),
            depends_on,
            auto_approve: through.get(at).copied().unwrap_or(false),
        });
    }
    CiRequest::Onboard {
        repo: name.clone(),
        title: format!("Onboard {name}"),
        body,
        spec: OnboardSpec {
            url: format!("https://{origin}"),
            default_branch: branch.to_owned(),
            grants: Default::default(),
            prerequisites: Vec::new(),
        },
        tickets,
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
                        draft.through = vec![false; advice.cards.len()];
                        draft.summary = Some(markdown::Content::parse(&advice.summary));
                        draft.bodies = advice
                            .cards
                            .iter()
                            .map(|card| markdown::Content::parse(&card.body))
                            .collect();
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
            OnboardMsg::Mode(at) => {
                if let Some(through) = draft.through.get_mut(at) {
                    *through = !*through;
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
        let branch = self.default_branch_of(origin);
        let Some(draft) = self.page.onboard.get_mut(origin) else {
            return Task::none();
        };
        let request = onboarding(
            origin,
            &branch,
            draft.advice.as_ref(),
            &draft.cards,
            &draft.through,
        );
        draft.filing = true;
        draft.refused = None;
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

        // Where it stands, and the plan's title over it.
        let mut facts = vec![origin.to_owned(), "not built by a CI".to_owned()];
        if let Some(listed) = &listed {
            facts.push(format!("default branch {}", listed.default_branch));
            if listed.checkout {
                facts.push(format!("checked out in ~/work/{origin}"));
            }
        }
        let title = advice
            .map(|a| a.title.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(origin);
        let mut head = row![
            column![
                text(title).size(22),
                text(facts.join(" \u{b7} ")).size(CAPTION_TEXT).color(muted),
            ]
            .spacing(6)
            .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if let Some(advice) = advice.filter(|a| !a.model.is_empty()) {
            head = head.push(
                text(format!(
                    "plan by {}, {}",
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
        let mut body = column![head].spacing(20);
        let Some(ci) = self.ci.keys().next() else {
            body = body.push(text("No CI configured").size(ROW_TEXT).color(muted));
            return page(body);
        };
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
        if let Some(error) = advice.and_then(|a| a.error.as_ref()) {
            body = body.push(
                text(format!("No plan: {error}"))
                    .size(CAPTION_TEXT)
                    .color(muted),
            );
        }
        if let Some(summary) = &draft.summary {
            let ci = ci.clone();
            body = body.push(
                container(
                    markdown::view(summary.items(), self.markdown_settings())
                        .map(move |uri| Message::Ci(ci.clone(), CiMsg::OpenLink(uri))),
                )
                .max_width(760),
            );
        }
        if !draft.cards.is_empty() {
            body = body.push(caption("The plan, one ticket per chosen section", muted));
            let mut cards = row![].spacing(12);
            let mut place = 0;
            for (at, card) in draft.cards.iter().enumerate() {
                let step = card.chosen.then(|| {
                    place += 1;
                    place
                });
                cards = cards.push(self.card_view(t, ci, origin, draft, at, step));
            }
            body = body.push(cards.wrap().vertical_spacing(12));
        }
        body = body
            .push(rule::horizontal(1))
            .push(self.preview(t, origin, draft));
        page(body)
    }

    /// One card: its place among the chosen ones, its heading and text,
    /// and its mode. A press chooses or drops it; a press on the mode
    /// switches it.
    fn card_view<'a>(
        &'a self,
        t: &Tokens,
        ci: &'a crate::workspace::RunnerKey,
        origin: &'a str,
        draft: &'a Draft,
        at: usize,
        step: Option<usize>,
    ) -> Element<'a, Message, Theme> {
        let send = move |message: OnboardMsg| Message::Onboard(origin.to_owned(), message);
        let (muted, accent, surface) = (self.theme.muted(), self.theme.accent(), t.surface);
        let card = &draft.cards[at];
        let chosen = card.chosen;
        let through = draft.through.get(at).copied().unwrap_or(false);
        let number = container(
            text(step.map_or_else(|| "\u{2013}".to_owned(), |n| n.to_string()))
                .size(CAPTION_TEXT)
                .color(if chosen { surface } else { muted }),
        )
        .center_x(22)
        .center_y(22)
        .style(move |_: &Theme| container::Style {
            background: chosen.then(|| accent.into()),
            border: Border {
                color: if chosen { accent } else { muted },
                width: 1.0,
                radius: 11.0.into(),
            },
            ..container::Style::default()
        });
        let mode =
            button(text(if through { "runs through" } else { "review" }).size(11)).padding([1, 8]);
        let mode = if through {
            mode.style(style::pill_button(t))
        } else {
            mode.style(style::secondary_button(t))
        };
        let mode = mode.on_press_maybe(chosen.then(|| send(OnboardMsg::Mode(at))));
        let title_color = if chosen {
            self.theme.extended().background.base.text
        } else {
            muted
        };
        let head = row![
            number,
            text(card.title.as_str())
                .size(15)
                .color(title_color)
                .width(Length::Fill),
            mode,
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let mut lines = column![head].spacing(8);
        if let Some(content) = draft.bodies.get(at) {
            let ci = ci.clone();
            let faded = muted.scale_alpha(0.8);
            lines = lines.push(
                container(
                    markdown::view(content.items(), self.markdown_settings())
                        .map(move |uri| Message::Ci(ci.clone(), CiMsg::OpenLink(uri))),
                )
                .style(move |_: &Theme| container::Style {
                    text_color: (!chosen).then_some(faded),
                    ..container::Style::default()
                }),
            );
        }
        let tile = container(lines)
            .width(CARD_WIDTH)
            .padding(14)
            .style(move |_: &Theme| container::Style {
                background: Some(if chosen {
                    accent.scale_alpha(0.06).into()
                } else {
                    surface.into()
                }),
                border: Border {
                    color: if chosen {
                        accent
                    } else {
                        muted.scale_alpha(0.3)
                    },
                    width: if chosen { 1.5 } else { 1.0 },
                    radius: 8.0.into(),
                },
                ..container::Style::default()
            });
        mouse_area(tile)
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press(send(OnboardMsg::Card(at)))
            .into()
    }

    /// What `Onboard` files, numbered as the CI will number it in a new
    /// repository: the request, then the chosen sections in order; and the
    /// button.
    fn preview<'a>(
        &'a self,
        t: &Tokens,
        origin: &'a str,
        draft: &'a Draft,
    ) -> Element<'a, Message, Theme> {
        let send = move |message: OnboardMsg| Message::Onboard(origin.to_owned(), message);
        let muted = self.theme.muted();
        let mut list = column![
            caption("Onboard files", muted),
            text(format!(
                "1. Onboarding request for `{}`, branch {}, approved",
                repo_name(origin),
                self.default_branch_of(origin)
            ))
            .size(ROW_TEXT),
        ]
        .spacing(4);
        let chosen: Vec<(usize, &Card)> = draft
            .cards
            .iter()
            .enumerate()
            .filter(|(_, card)| card.chosen)
            .collect();
        for (place, (at, card)) in chosen.iter().enumerate() {
            let through = draft.through.get(*at).copied().unwrap_or(false);
            let mut line = format!(
                "{}. {} \u{2014} {}",
                place + 2,
                card.title,
                if through {
                    "runs through"
                } else {
                    "plan for review"
                }
            );
            if place > 0 {
                line.push_str(&format!(", after {}", place + 1));
            }
            list = list.push(text(line).size(ROW_TEXT));
        }
        let refusal = draft.advice.as_ref().and_then(|a| a.refusal.as_ref());
        let words = match (draft.filing, chosen.len()) {
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
                 tickets are planned one after the other."
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
    scroll::vertical(
        container(body.max_width(MAX_WIDTH).padding(24))
            .width(Length::Fill)
            .center_x(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}

/// A caption over a part of the Board.
fn caption<'a>(words: &str, muted: iced::Color) -> Element<'a, Message, Theme> {
    text(words.to_uppercase())
        .size(CAPTION_TEXT)
        .color(muted)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(title: &str, chosen: bool) -> Card {
        Card {
            title: title.to_owned(),
            body: format!("Do {title}.\n"),
            chosen,
        }
    }

    #[test]
    fn the_chosen_sections_become_tickets_each_after_the_one_before() {
        let cards = [
            card("Jobs", true),
            card("Docs", false),
            card("Deploy", true),
            card("Follow", true),
        ];
        let through = [true, true, false, true];
        let CiRequest::Onboard {
            repo,
            spec,
            tickets,
            ..
        } = onboarding("git.example.org/o/My_Site", "trunk", None, &cards, &through)
        else {
            panic!("an onboarding");
        };
        assert_eq!(repo, "my-site");
        assert_eq!(
            (spec.url.as_str(), spec.default_branch.as_str()),
            ("https://git.example.org/o/My_Site", "trunk")
        );
        let filed: Vec<(&str, Vec<usize>, bool)> = tickets
            .iter()
            .map(|t| (t.title.as_str(), t.depends_on.clone(), t.auto_approve))
            .collect();
        assert_eq!(
            filed,
            [
                ("Jobs", vec![], true),
                ("Deploy", vec![0], false),
                ("Follow", vec![1], true),
            ]
        );
        assert_eq!(
            tickets[1].body,
            "Do Deploy.\n\nFrom the onboarding plan of git.example.org/o/My_Site.\n"
        );
    }
}
