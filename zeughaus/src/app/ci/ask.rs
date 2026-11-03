//! An agent's open question in the editor: the form the Overview's detail
//! pane and the ticket popout show, what the user picks and types into it,
//! and the answer sent to the CI. The agent's terminal may answer first;
//! the ask then leaves with the next overview.

use iced::widget::{column, container, row, text, text_input};
use iced::{Alignment, Element, Font, Length, Task};
use zeughaus_link::{AskReply, AskView, CiRequest, check_replies};
use zeughaus_theme::Theme;

use super::{CiClient, SP_1, SP_2, SP_3, TEXT_MD, TEXT_SM, ago, now, plans};
use crate::app::App;
use crate::message::{CiMsg, Message};
use crate::style::{self, Tokens};
use crate::workspace::RunnerKey;

/// What the user has picked and typed for one ask, a slot per question.
pub(super) struct AskDraft {
    /// The chosen labels of each question, in option order.
    picked: Vec<Vec<String>>,
    custom: Vec<String>,
    sending: bool,
    error: Option<String>,
}

impl AskDraft {
    fn new(ask: &AskView) -> AskDraft {
        AskDraft {
            picked: vec![Vec::new(); ask.questions.len()],
            custom: vec![String::new(); ask.questions.len()],
            sending: false,
            error: None,
        }
    }

    /// The answer as the CI takes it.
    fn replies(&self, ask: &AskView) -> Vec<AskReply> {
        ask.questions
            .iter()
            .enumerate()
            .map(|(i, question)| {
                let picked = self.picked.get(i).map_or(&[][..], Vec::as_slice);
                let custom = self.custom.get(i).map_or("", |c| c.trim());
                AskReply {
                    id: question.id.clone(),
                    selected: question
                        .options
                        .iter()
                        .filter(|o| picked.contains(&o.label))
                        .map(|o| o.label.clone())
                        .collect(),
                    custom: (!custom.is_empty()).then(|| custom.to_owned()),
                }
            })
            .collect()
    }
}

impl CiClient {
    /// The open asks the last overview listed.
    pub(super) fn asks(&self) -> impl Iterator<Item = &AskView> {
        self.overview.iter().flat_map(|overview| &overview.asks)
    }

    /// The ask `id` and its draft, made on first use; `None` when the ask
    /// is gone.
    fn draft(&mut self, id: u64) -> Option<(&AskView, &mut AskDraft)> {
        let ask = self
            .overview
            .as_ref()?
            .asks
            .iter()
            .find(|ask| ask.id == id)?;
        let draft = self
            .ask_drafts
            .entry(id)
            .or_insert_with(|| AskDraft::new(ask));
        Some((ask, draft))
    }

    /// Picks or unpicks an option: a single-select question holds one
    /// label, which a second press clears; a multi-select one toggles it.
    pub(super) fn pick(&mut self, id: u64, question: usize, option: usize) {
        let Some((ask, draft)) = self.draft(id) else {
            return;
        };
        let Some(q) = ask.questions.get(question) else {
            return;
        };
        let Some(label) = q.options.get(option).map(|o| o.label.clone()) else {
            return;
        };
        let Some(picked) = draft.picked.get_mut(question) else {
            return;
        };
        if !q.multi {
            *picked = if picked.as_slice() == [label.clone()] {
                Vec::new()
            } else {
                vec![label]
            };
        } else if picked.contains(&label) {
            picked.retain(|l| *l != label);
        } else {
            picked.push(label);
            picked.sort_by_key(|l| q.options.iter().position(|o| o.label == *l));
        }
    }

    pub(super) fn type_custom(&mut self, id: u64, question: usize, text: String) {
        if let Some((_, draft)) = self.draft(id)
            && let Some(custom) = draft.custom.get_mut(question)
        {
            *custom = text;
        }
    }

    /// Sends the answer once it is complete and none is on its way.
    pub(super) fn send(&mut self, key: &RunnerKey, id: u64) -> Task<Message> {
        let endpoint = self.endpoint.clone();
        let Some((ask, draft)) = self.draft(id) else {
            return Task::none();
        };
        let replies = draft.replies(ask);
        if draft.sending || check_replies(&ask.questions, &replies).is_err() {
            return Task::none();
        }
        draft.sending = true;
        draft.error = None;
        let key = key.clone();
        Task::perform(
            plans::act(endpoint, CiRequest::AskAnswer { id, replies }),
            move |result| Message::Ci(key, CiMsg::AskSent { id, result }),
        )
    }

    /// What the CI answered: a refusal stays on the form; a success leaves
    /// with the next overview.
    pub(super) fn sent(&mut self, id: u64, result: Result<(), String>) {
        if let (Err(e), Some(draft)) = (result, self.ask_drafts.get_mut(&id)) {
            draft.sending = false;
            draft.error = Some(e);
        }
    }
}

impl App {
    /// An ask's questions, each with its options as pills and a box for an
    /// answer of the user's own, and Send.
    pub(super) fn ask_form<'a>(
        &'a self,
        ci: &'a RunnerKey,
        client: &'a CiClient,
        ask: &'a AskView,
    ) -> Element<'a, Message, Theme> {
        let t = Tokens::of(&self.theme);
        let muted = self.theme.muted();
        let draft = client.ask_drafts.get(&ask.id);
        let who = if ask.label.is_empty() {
            "An agent"
        } else {
            ask.label.as_str()
        };
        let mut form = column![
            text(format!("{who} asks · {}", ago(ask.asked, now())))
                .size(TEXT_SM)
                .color(muted)
        ]
        .spacing(SP_3);
        for (i, question) in ask.questions.iter().enumerate() {
            let picked = draft
                .and_then(|d| d.picked.get(i))
                .map_or(&[][..], Vec::as_slice);
            let mut part = column![].spacing(SP_1);
            if let Some(header) = &question.header {
                part = part.push(
                    container(text(header.as_str()).size(TEXT_SM))
                        .padding([0, 6])
                        .style(style::badge(&t, false)),
                );
            }
            part = part.push(text(question.question.as_str()).size(TEXT_MD));
            for (o, option) in question.options.iter().enumerate() {
                let label = if question.recommended == Some(o) {
                    format!("{} (Recommended)", option.label)
                } else {
                    option.label.clone()
                };
                let pill = self
                    .ticket_pill(label, picked.contains(&option.label))
                    .on_press(Message::Ci(
                        ci.clone(),
                        CiMsg::AskPick {
                            id: ask.id,
                            question: i,
                            option: o,
                        },
                    ));
                let mut line = row![pill].spacing(SP_2).align_y(Alignment::Center);
                if let Some(description) = &option.description {
                    line = line.push(
                        text(description.as_str())
                            .size(TEXT_SM)
                            .color(muted)
                            .width(Length::Fill),
                    );
                }
                part = part.push(line);
                if let Some(preview) = option.preview.as_deref().filter(|p| !p.is_empty()) {
                    part = part.push(text(preview).size(TEXT_SM).font(Font::MONOSPACE));
                }
            }
            let custom = draft
                .and_then(|d| d.custom.get(i))
                .map_or("", String::as_str);
            let id = ask.id;
            part = part.push(text_input("Other answer", custom).size(TEXT_SM).on_input(
                move |text| {
                    Message::Ci(
                        ci.clone(),
                        CiMsg::AskCustom {
                            id,
                            question: i,
                            text,
                        },
                    )
                },
            ));
            form = form.push(part);
        }
        let sending = draft.is_some_and(|d| d.sending);
        let complete = match draft {
            Some(draft) => check_replies(&ask.questions, &draft.replies(ask)).is_ok(),
            None => check_replies(&ask.questions, &AskDraft::new(ask).replies(ask)).is_ok(),
        };
        form = form.push(
            self.ticket_pill(if sending { "Sending" } else { "Send" }, true)
                .on_press_maybe(
                    (complete && !sending)
                        .then(|| Message::Ci(ci.clone(), CiMsg::AskSend { id: ask.id })),
                ),
        );
        if let Some(error) = draft.and_then(|d| d.error.as_deref()) {
            form = form.push(text(error).size(TEXT_SM).color(self.theme.error()));
        }
        form.into()
    }
}
