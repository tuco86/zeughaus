//! The Board of a project no CI builds: what it is and where it stands,
//! the ways it could join the CI, and the onboarding request it files.
//!
//! Showing the Board asks the local runner for the facts about the project
//! (its listing, its work checkout or its forge) and hands them to the
//! first CI, whose `[advice]` model answers with a summary, status lines
//! and operations. Picking an operation fills the form with the settings
//! it would file; the form files the onboarding request, which the owner
//! approves on the same Board once the CI lists the pending repository.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use iced::widget::{
    Space, button, checkbox, column, container, mouse_area, pick_list, row, rule, scrollable, text,
    text_input,
};
use iced::{Alignment, Border, Element, Length, Task};
use zeughaus_link::{
    AdviceView, CiReply, CiRequest, DocKind, OnboardSettings, OnboardSpec, Operation,
    ProjectsRequest,
};
use zeughaus_theme::Theme;

use super::{ago, now};
use crate::app::App;
use crate::message::{Message, OnboardEvent, OnboardMsg};
use crate::style::{self, Tokens};

/// The widest the Board's column grows.
const MAX_WIDTH: f32 = 960.0;
/// Text of a line, and of a caption.
const ROW_TEXT: f32 = 13.0;
const CAPTION_TEXT: f32 = 12.0;
/// The width of a form label.
const LABEL_WIDTH: f32 = 130.0;
/// The pick list entry for following nothing.
const FOLLOWS_NOTHING: &str = "nothing";

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
    /// Why no advice arrived.
    error: Option<String>,
    /// The operation whose settings the form took.
    picked: Option<usize>,
    /// The user changed the form: arriving advice no longer fills it.
    touched: bool,
    form: Form,
    filing: bool,
    /// Why the last request was not filed.
    refused: Option<String>,
}

/// The onboarding request as the form holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Form {
    name: String,
    branch: String,
    push: bool,
    tag: bool,
    nightly: bool,
    /// The repository of the CI it follows (`push <branch> of <repo>`);
    /// empty for none.
    follows: String,
    platforms: BTreeSet<String>,
    /// `NAME: event, event; OTHER: event`.
    grants: String,
    /// `;`-separated.
    prerequisites: String,
    notes: String,
}

impl Form {
    /// A first draft: the repository's name, its default branch, pushes to
    /// it.
    fn new(origin: &str, branch: &str) -> Form {
        Form {
            name: name_of(origin),
            branch: branch.to_owned(),
            push: true,
            ..Form::default()
        }
    }

    /// The form for an operation's settings; the notes stay.
    fn from_settings(settings: &OnboardSettings, origin: &str, notes: String) -> Form {
        let branch = match settings.default_branch.trim() {
            "" => "main".to_owned(),
            branch => branch.to_owned(),
        };
        let mut form = Form {
            name: if settings.name.is_empty() {
                name_of(origin)
            } else {
                settings.name.clone()
            },
            branch: branch.clone(),
            platforms: settings.platforms.iter().cloned().collect(),
            grants: settings
                .grants
                .iter()
                .map(|(secret, events)| format!("{secret}: {}", events.join(", ")))
                .collect::<Vec<_>>()
                .join("; "),
            prerequisites: settings.prerequisites.join("; "),
            notes,
            ..Form::default()
        };
        for event in &settings.events {
            let words: Vec<&str> = event.split_whitespace().collect();
            match words[..] {
                ["push", b] if b == branch => form.push = true,
                ["tag", "v*"] => form.tag = true,
                ["nightly"] => form.nightly = true,
                ["push", _, "of", repo] => form.follows = repo.to_owned(),
                _ => {}
            }
        }
        form
    }

    /// The event patterns the form selects.
    fn events(&self) -> Vec<String> {
        let branch = self.branch.trim();
        let mut events = Vec::new();
        if self.push {
            events.push(format!("push {branch}"));
        }
        if self.tag {
            events.push("tag v*".to_owned());
        }
        if self.nightly {
            events.push("nightly".to_owned());
        }
        if !self.follows.is_empty() {
            events.push(format!("push main of {}", self.follows));
        }
        events
    }

    /// The request's title, body and spec, or what is wrong with the form.
    fn request(
        &self,
        origin: &str,
        operation: Option<&Operation>,
    ) -> Result<(String, String, OnboardSpec), String> {
        let name = self.name.trim();
        if !valid_name(name) {
            return Err(format!(
                "the name `{name}` must be lowercase letters, digits and dashes"
            ));
        }
        let branch = self.branch.trim();
        if branch.is_empty() || branch.contains(char::is_whitespace) {
            return Err("the default branch is one word".to_owned());
        }
        let events = self.events();
        if events.is_empty() {
            return Err("pick at least one event".to_owned());
        }
        let grants = parse_grants(&self.grants)?;
        let prerequisites: Vec<String> = self
            .prerequisites
            .split(';')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let mut body = format!("Onboarding requested from the editor for {origin}.\n\n");
        if !self.notes.trim().is_empty() {
            body.push_str(self.notes.trim());
            body.push_str("\n\n");
        }
        body.push_str(&format!("- Events: {}\n", events.join(", ")));
        if self.platforms.is_empty() {
            body.push_str("- Platforms: none named\n");
        } else {
            let platforms: Vec<&str> = self.platforms.iter().map(String::as_str).collect();
            body.push_str(&format!("- Platforms: {}\n", platforms.join(", ")));
        }
        for (secret, granted) in &grants {
            body.push_str(&format!("- Secret {secret} for {}\n", granted.join(", ")));
        }
        for line in &prerequisites {
            body.push_str(&format!("- Host prerequisite: {line}\n"));
        }
        let title = match operation {
            Some(operation) => {
                body.push_str(&format!(
                    "\n## {} ({})\n\n{}\n",
                    operation.title,
                    operation.kind.name(),
                    operation.body.trim()
                ));
                format!("Onboard {name}: {}", operation.title)
            }
            None => format!("Onboard {name}"),
        };
        let spec = OnboardSpec {
            url: format!("https://{origin}"),
            default_branch: branch.to_owned(),
            grants,
            prerequisites,
        };
        Ok((title, body, spec))
    }
}

/// `[a-z0-9][a-z0-9-]*`, the CI's repository names.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The last segment of `origin` as a repository name.
fn name_of(origin: &str) -> String {
    origin
        .rsplit('/')
        .next()
        .unwrap_or(origin)
        .to_lowercase()
        .replace(['_', '.'], "-")
}

/// `NAME: event, event; OTHER: event` as grants.
fn parse_grants(text: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut grants = BTreeMap::new();
    for part in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((secret, events)) = part.split_once(':') else {
            return Err(format!("`{part}`: a secret is `NAME: event, event`"));
        };
        let secret = secret.trim();
        let mut chars = secret.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            return Err(format!("`{secret}`: a secret name is [A-Z_][A-Z0-9_]*"));
        }
        let events: Vec<String> = events
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect();
        if events.is_empty() {
            return Err(format!("`{secret}` is granted to no event"));
        }
        grants.insert(secret.to_owned(), events);
    }
    Ok(grants)
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
        let branch = self.default_branch_of(origin);
        let draft = self
            .page
            .onboard
            .entry(origin.to_owned())
            .or_insert_with(|| Draft {
                form: Form::new(origin, &branch),
                ..Draft::default()
            });
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
            OnboardMsg::File => return self.file_onboarding(&origin),
            _ => {}
        }
        let Some(draft) = self.page.onboard.get_mut(&origin) else {
            return Task::none();
        };
        let edited = matches!(
            message,
            OnboardMsg::Name(_)
                | OnboardMsg::Branch(_)
                | OnboardMsg::Event(..)
                | OnboardMsg::Follows(_)
                | OnboardMsg::Platform(..)
                | OnboardMsg::Grants(_)
                | OnboardMsg::Prerequisites(_)
                | OnboardMsg::Notes(_)
        );
        match message {
            OnboardMsg::Advice(result) => {
                draft.asking = false;
                match result {
                    Ok(advice) => {
                        draft.error = None;
                        let advice = *advice;
                        if !draft.touched && !advice.operations.is_empty() {
                            draft.form = Form::from_settings(
                                &advice.operations[0].settings,
                                &origin,
                                std::mem::take(&mut draft.form.notes),
                            );
                            draft.picked = Some(0);
                        }
                        draft.advice = Some(advice);
                    }
                    Err(e) => draft.error = Some(e),
                }
            }
            OnboardMsg::Pick(at) => {
                if let Some(operation) = draft.advice.as_ref().and_then(|a| a.operations.get(at)) {
                    draft.form = Form::from_settings(
                        &operation.settings,
                        &origin,
                        std::mem::take(&mut draft.form.notes),
                    );
                    draft.picked = Some(at);
                    draft.touched = true;
                }
            }
            OnboardMsg::Name(name) => draft.form.name = name,
            OnboardMsg::Branch(branch) => draft.form.branch = branch,
            OnboardMsg::Event(event, on) => match event {
                OnboardEvent::Push => draft.form.push = on,
                OnboardEvent::Tag => draft.form.tag = on,
                OnboardEvent::Nightly => draft.form.nightly = on,
            },
            OnboardMsg::Follows(repo) => {
                draft.form.follows = if repo == FOLLOWS_NOTHING {
                    String::new()
                } else {
                    repo
                };
            }
            OnboardMsg::Platform(platform, on) => {
                if on {
                    draft.form.platforms.insert(platform);
                } else {
                    draft.form.platforms.remove(&platform);
                }
            }
            OnboardMsg::Grants(grants) => draft.form.grants = grants,
            OnboardMsg::Prerequisites(text) => draft.form.prerequisites = text,
            OnboardMsg::Notes(notes) => draft.form.notes = notes,
            OnboardMsg::Filed(result) => {
                draft.filing = false;
                match result {
                    // The CI lists the pending repository from now on, and
                    // its Board takes this one's place.
                    Ok(_) => {
                        self.page.onboard.remove(&origin);
                        return self.poll_ci();
                    }
                    Err(e) => draft.refused = Some(e),
                }
                return Task::none();
            }
            OnboardMsg::Facts(_) | OnboardMsg::Refresh | OnboardMsg::File => {}
        }
        if edited {
            draft.touched = true;
        }
        Task::none()
    }

    /// Files the form as an onboarding request with the first CI.
    fn file_onboarding(&mut self, origin: &str) -> Task<Message> {
        let Some(client) = self.ci.values().next() else {
            return Task::none();
        };
        let endpoint = client.endpoint.clone();
        let Some(draft) = self.page.onboard.get_mut(origin) else {
            return Task::none();
        };
        let operation = draft
            .picked
            .and_then(|at| draft.advice.as_ref()?.operations.get(at));
        let (title, body, spec) = match draft.form.request(origin, operation) {
            Ok(request) => request,
            Err(e) => {
                draft.refused = Some(e);
                return Task::none();
            }
        };
        draft.filing = true;
        draft.refused = None;
        let name = draft.form.name.trim().to_owned();
        let request = CiRequest::DocCreate {
            repo: name.clone(),
            kind: DocKind::Onboarding,
            title,
            body,
            source: name,
            links: Vec::new(),
            runs_on: Vec::new(),
            onboard: Some(spec),
        };
        let origin = origin.to_owned();
        Task::perform(
            async move {
                match crate::feed::ci(endpoint, request).await? {
                    CiReply::DocCreated { number } => Ok(number),
                    _ => Err(super::UNEXPECTED.to_owned()),
                }
            },
            move |result| Message::Onboard(origin.clone(), OnboardMsg::Filed(result)),
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
        match (draft, advice) {
            (Some(draft), None) if draft.asking => {
                body = body.push(
                    text("Looking at the project\u{2026}")
                        .size(ROW_TEXT)
                        .color(muted),
                );
            }
            (None, _) => {
                body = body.push(
                    button(text("Ask the CI about it").size(CAPTION_TEXT))
                        .padding([4, 14])
                        .style(style::pill_button(t))
                        .on_press(send(OnboardMsg::Refresh)),
                );
                return page(body);
            }
            _ => {}
        }
        let Some(draft) = draft else {
            return page(body);
        };
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
            if let Some(refusal) = &advice.refusal {
                body = body.push(
                    text(format!("Cannot be onboarded as it is: {refusal}"))
                        .size(ROW_TEXT)
                        .color(self.theme.readable(self.theme.hint())),
                );
            }
            if let Some(error) = &advice.error {
                body = body.push(
                    text(format!("No advice: {error}"))
                        .size(CAPTION_TEXT)
                        .color(muted),
                );
            }
            if !advice.operations.is_empty() {
                body = body.push(caption("Ways to join", muted));
                let mut cards = row![].spacing(12);
                for (at, operation) in advice.operations.iter().enumerate() {
                    cards = cards.push(operation_card(
                        t,
                        &self.theme,
                        operation,
                        draft.picked == Some(at),
                        send(OnboardMsg::Pick(at)),
                    ));
                }
                body = body.push(cards.wrap().vertical_spacing(12));
            }
        }
        body = body
            .push(rule::horizontal(1))
            .push(caption("Onboarding request", muted))
            .push(self.onboard_form(t, origin, draft));
        page(body)
    }

    /// The settings the request files, and the button that files it.
    fn onboard_form<'a>(
        &'a self,
        t: &Tokens,
        origin: &'a str,
        draft: &'a Draft,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let send = move |message: OnboardMsg| Message::Onboard(origin.to_owned(), message);
        let form = &draft.form;
        let label = |words: &'a str| {
            text(words)
                .size(CAPTION_TEXT)
                .color(muted)
                .width(LABEL_WIDTH)
        };
        let field = |words: &'a str, input: Element<'a, Message, Theme>| {
            row![label(words), input]
                .spacing(8)
                .align_y(Alignment::Center)
        };
        let input = |placeholder: &'a str, value: &'a str, on: fn(String) -> OnboardMsg| {
            text_input(placeholder, value)
                .size(ROW_TEXT)
                .padding([4, 8])
                .on_input(move |value| send(on(value)))
        };
        let client = self.ci.values().next();
        let repos: Vec<String> = std::iter::once(FOLLOWS_NOTHING.to_owned())
            .chain(
                client
                    .into_iter()
                    .flat_map(|c| c.repo_docs())
                    .map(|repo| repo.repo.clone()),
            )
            .collect();
        let platforms: Vec<String> = client
            .and_then(|c| c.overview.as_ref())
            .map(|o| o.platforms.iter().map(|(p, _)| p.clone()).collect())
            .unwrap_or_default();

        let branch = form.branch.trim();
        let events = row![
            checkbox(form.push)
                .label(format!("push {branch}"))
                .text_size(ROW_TEXT)
                .on_toggle(move |on| send(OnboardMsg::Event(OnboardEvent::Push, on))),
            checkbox(form.tag)
                .label("tag v*")
                .text_size(ROW_TEXT)
                .on_toggle(move |on| send(OnboardMsg::Event(OnboardEvent::Tag, on))),
            checkbox(form.nightly)
                .label("nightly")
                .text_size(ROW_TEXT)
                .on_toggle(move |on| send(OnboardMsg::Event(OnboardEvent::Nightly, on))),
        ]
        .spacing(16);
        let follows = pick_list(
            repos,
            Some(if form.follows.is_empty() {
                FOLLOWS_NOTHING.to_owned()
            } else {
                form.follows.clone()
            }),
            move |repo: String| send(OnboardMsg::Follows(repo)),
        )
        .text_size(ROW_TEXT)
        .padding([4, 8]);
        let mut platform_boxes = row![].spacing(16);
        for platform in platforms {
            let on = form.platforms.contains(&platform);
            let name = platform.clone();
            platform_boxes = platform_boxes.push(
                checkbox(on)
                    .label(platform)
                    .text_size(ROW_TEXT)
                    .on_toggle(move |on| send(OnboardMsg::Platform(name.clone(), on))),
            );
        }
        let refusal = draft.advice.as_ref().and_then(|a| a.refusal.as_ref());
        let file = button(
            text(if draft.filing {
                "Filing\u{2026}"
            } else {
                "File onboarding request"
            })
            .size(CAPTION_TEXT),
        )
        .padding([5, 16])
        .style(style::pill_button(t))
        .on_press_maybe((!draft.filing && refusal.is_none()).then(|| send(OnboardMsg::File)));
        let mut footer = row![
            file,
            text(
                "Approve it on this Board once it is filed: the CI then adds its bot and push \
                 hook and builds the default branch."
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
        column![
            field(
                "Name",
                input("name in ci.toml", &form.name, OnboardMsg::Name).into()
            ),
            field(
                "Default branch",
                input("main", &form.branch, OnboardMsg::Branch).into()
            ),
            field("Events", events.into()),
            field("Follows", follows.into()),
            field(
                "Platforms",
                platform_boxes.wrap().vertical_spacing(8).into()
            ),
            field(
                "Secrets",
                input(
                    "DEPLOY_KEY: push main; OTHER_TOKEN: tag v*",
                    &form.grants,
                    OnboardMsg::Grants
                )
                .into()
            ),
            field(
                "Host prerequisites",
                input(
                    "set up by hand, separated by ;",
                    &form.prerequisites,
                    OnboardMsg::Prerequisites
                )
                .into()
            ),
            field(
                "Notes",
                input(
                    "what the pipeline should do",
                    &form.notes,
                    OnboardMsg::Notes
                )
                .into()
            ),
            footer,
        ]
        .spacing(10)
        .into()
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

/// One way to join: its kind, title and body; pressing it takes its
/// settings into the form.
fn operation_card<'a>(
    t: &Tokens,
    theme: &Theme,
    operation: &'a Operation,
    picked: bool,
    press: Message,
) -> Element<'a, Message, Theme> {
    let (muted, accent, surface) = (theme.muted(), theme.accent(), t.surface);
    let chip = container(text(operation.kind.name()).size(11))
        .padding([0, 6])
        .style(style::badge(t, picked));
    let card = container(
        column![
            row![chip, text(operation.title.as_str()).size(ROW_TEXT)]
                .spacing(6)
                .align_y(Alignment::Center),
            text(operation.body.as_str())
                .size(CAPTION_TEXT)
                .color(muted),
            Space::new().height(0),
        ]
        .spacing(6),
    )
    .width(280)
    .padding(10)
    .style(move |_: &Theme| container::Style {
        background: Some(surface.into()),
        border: Border {
            color: if picked {
                accent
            } else {
                muted.scale_alpha(0.3)
            },
            width: if picked { 2.0 } else { 1.0 },
            radius: 6.0.into(),
        },
        ..container::Style::default()
    });
    mouse_area(card)
        .interaction(iced::mouse::Interaction::Pointer)
        .on_press(press)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::OperationKind;

    #[test]
    fn grants_are_named_secrets_with_their_events() {
        assert_eq!(
            parse_grants(" DEPLOY_KEY: push main, tag v* ;; TOKEN:nightly"),
            Ok([
                (
                    "DEPLOY_KEY".to_owned(),
                    vec!["push main".to_owned(), "tag v*".to_owned()]
                ),
                ("TOKEN".to_owned(), vec!["nightly".to_owned()]),
            ]
            .into())
        );
        assert_eq!(
            parse_grants("deploy: push main"),
            Err("`deploy`: a secret name is [A-Z_][A-Z0-9_]*".to_owned())
        );
        assert_eq!(
            parse_grants("KEY:"),
            Err("`KEY` is granted to no event".to_owned())
        );
    }

    #[test]
    fn an_operation_fills_the_form_and_the_form_files_its_request() {
        let operation = Operation {
            kind: OperationKind::Follow,
            title: "Rebuild with weida".to_owned(),
            body: "Builds the site.".to_owned(),
            settings: OnboardSettings {
                name: "weida-site".to_owned(),
                default_branch: "trunk".to_owned(),
                events: vec!["push trunk".to_owned(), "push main of weida".to_owned()],
                platforms: vec!["linux-amd64".to_owned()],
                grants: [("DEPLOY".to_owned(), vec!["push trunk".to_owned()])].into(),
                prerequisites: vec!["a served directory".to_owned()],
            },
        };
        let origin = "git.doodleshnookie.net/tuco86/weida-site";
        let form = Form::from_settings(&operation.settings, origin, "Keep it small.".to_owned());
        assert_eq!(form.events(), ["push trunk", "push main of weida"]);
        let (title, body, spec) = form.request(origin, Some(&operation)).unwrap();
        assert_eq!(title, "Onboard weida-site: Rebuild with weida");
        assert_eq!(
            body,
            "Onboarding requested from the editor for git.doodleshnookie.net/tuco86/weida-site.\n\n\
             Keep it small.\n\n\
             - Events: push trunk, push main of weida\n\
             - Platforms: linux-amd64\n\
             - Secret DEPLOY for push trunk\n\
             - Host prerequisite: a served directory\n\
             \n## Rebuild with weida (follow)\n\nBuilds the site.\n"
        );
        assert_eq!(
            spec,
            OnboardSpec {
                url: format!("https://{origin}"),
                default_branch: "trunk".to_owned(),
                grants: [("DEPLOY".to_owned(), vec!["push trunk".to_owned()])].into(),
                prerequisites: vec!["a served directory".to_owned()],
            }
        );
    }

    #[test]
    fn a_form_without_events_or_with_a_bad_name_is_not_filed() {
        let mut form = Form::new("h/o/Scan_RS", "main");
        assert_eq!(form.name, "scan-rs");
        form.push = false;
        assert_eq!(
            form.request("h/o/Scan_RS", None).unwrap_err(),
            "pick at least one event"
        );
        form.push = true;
        form.name = "Scan RS".to_owned();
        assert!(form.request("h/o/Scan_RS", None).is_err());
    }
}
