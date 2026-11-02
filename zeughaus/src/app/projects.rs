//! The Projects section of the tab bar: one locked group per project the
//! user opened or that has tabs on a runner, whatever runner they run on.
//!
//! A group holds the project's Board, its work shell (or a button that
//! starts one), its masters, its plan sessions, the rest of its runner tabs
//! and the channel views the user opened, in that order. The runner tabs
//! come from the runners' project groups (`GroupSnapshot::project`); the
//! Board, the button and the views are the editor's own and carry ids it
//! mints, under [`RunnerKey::PROJECTS`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use iced::Task;
use zeughaus_link::{ActivityKind, ProjectView, ProjectsReply};
use zeughaus_mux::{GroupId, GroupSnapshot, TabId, TerminalId, TopologyCommand};

use super::App;
use super::PageView;
use crate::message::Message;
use crate::notices::Severity;
use crate::workspace::{CollapseKey, RunnerKey, TabRef};

/// How long the palette goes without asking the local runner for its
/// projects again.
const ASK_AGAIN: Duration = Duration::from_secs(300);

/// A view the user opened in a project's group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ViewKey {
    /// A CI channel, by the CI's key and the channel's view id.
    Channel(RunnerKey, u64),
}

/// What an editor-made tab or group of the section is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ViewSlot {
    /// A project's Board; its id also names the project's group.
    Board(String),
    /// The button that opens the work shell.
    Start(String),
    View(String, ViewKey),
}

/// What a runner tab in a project group shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TabKind {
    /// The user's shell in the work checkout.
    Work,
    Master,
    Plan,
    Other,
}

/// One runner's project groups, in the order the runner shows them.
pub(super) struct RunnerGroups<'a> {
    pub key: RunnerKey,
    /// This machine's runner, whose tabs come first.
    pub local: bool,
    pub groups: Vec<&'a GroupSnapshot>,
}

/// What [`compose`] works from.
pub(super) struct ComposeInput<'a, K, N> {
    /// The projects the user opened.
    pub open: &'a BTreeSet<String>,
    /// The runner tabs a project's group held when the user closed it: it
    /// stays away until a tab not among them shows up.
    pub dismissed: &'a HashMap<String, BTreeSet<(RunnerKey, TabId)>>,
    /// In section order.
    pub runners: Vec<RunnerGroups<'a>>,
    pub kind_of: K,
    /// A project's name by its origin.
    pub names: N,
    /// The views the user opened, in opening order.
    pub views: &'a [(String, ViewKey)],
}

/// One tab of a project's group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GroupTab {
    Board,
    /// The button that starts the work shell, while no runner has one.
    Start,
    Runner(TabRef),
    View(ViewKey),
}

/// A project's group as the bar draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectGroup {
    pub origin: String,
    pub name: String,
    pub tabs: Vec<GroupTab>,
}

/// The project groups of the bar, by name: what is open, and what has a
/// runner tab that was not there when its group was closed.
pub(super) fn compose<K, N>(input: &ComposeInput<'_, K, N>) -> Vec<ProjectGroup>
where
    K: Fn(&RunnerKey, TabId) -> TabKind,
    N: Fn(&str) -> String,
{
    // The local runner first, then the sections in their order (a stable
    // sort keeps it).
    let mut runners: Vec<&RunnerGroups<'_>> = input.runners.iter().collect();
    runners.sort_by_key(|runner| !runner.local);
    let mut by_origin: BTreeMap<&str, Vec<(TabKind, TabRef)>> = BTreeMap::new();
    for runner in runners {
        for group in &runner.groups {
            let Some(origin) = group.project.as_deref() else {
                continue;
            };
            for tab in &group.tabs {
                by_origin.entry(origin).or_default().push((
                    (input.kind_of)(&runner.key, tab.id),
                    TabRef {
                        runner: runner.key.clone(),
                        tab: tab.id,
                    },
                ));
            }
        }
    }
    let mut visible: BTreeSet<&str> = input.open.iter().map(String::as_str).collect();
    for (origin, tabs) in &by_origin {
        let dismissed = input.dismissed.get(*origin);
        let new = tabs.iter().any(|(_, tab)| {
            dismissed.is_none_or(|gone| !gone.contains(&(tab.runner.clone(), tab.tab)))
        });
        if new {
            visible.insert(origin);
        }
    }
    let mut groups: Vec<ProjectGroup> = visible
        .into_iter()
        .map(|origin| {
            let runner_tabs = by_origin.get(origin).map_or(&[][..], Vec::as_slice);
            let of_kind = |kind: TabKind| {
                runner_tabs
                    .iter()
                    .filter(move |(k, _)| *k == kind)
                    .map(|(_, tab)| GroupTab::Runner(tab.clone()))
            };
            let mut tabs = vec![GroupTab::Board];
            let work: Vec<GroupTab> = of_kind(TabKind::Work).collect();
            if work.is_empty() {
                tabs.push(GroupTab::Start);
            }
            tabs.extend(work);
            tabs.extend(of_kind(TabKind::Master));
            tabs.extend(of_kind(TabKind::Plan));
            tabs.extend(of_kind(TabKind::Other));
            tabs.extend(
                input
                    .views
                    .iter()
                    .filter(|(o, _)| o == origin)
                    .map(|(_, view)| GroupTab::View(view.clone())),
            );
            ProjectGroup {
                origin: origin.to_owned(),
                name: (input.names)(origin),
                tabs,
            }
        })
        .collect();
    groups.sort_by(|a, b| (&a.name, &a.origin).cmp(&(&b.name, &b.origin)));
    groups
}

/// A tab of the bar in the order it is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BarTab {
    Runner(TabRef),
    Project(TabId),
}

/// The Projects section's state in this window.
#[derive(Default)]
pub(super) struct ProjectsUi {
    /// The projects the user opened, saved with the preferences.
    pub(super) open: BTreeSet<String>,
    pub(super) dismissed: HashMap<String, BTreeSet<(RunnerKey, TabId)>>,
    /// The views opened in a project's group, in opening order.
    pub(super) views: Vec<(String, ViewKey)>,
    ids: HashMap<ViewSlot, u64>,
    slots: HashMap<u64, ViewSlot>,
    next_id: u64,
    /// What [`compose`] made of the workspace after the last update.
    pub(super) groups: Vec<ProjectGroup>,
    /// The projects whose group was composed in this window before: a new
    /// one starts collapsed while the bar is on top.
    seen: HashSet<String>,
    /// What the local runner last listed.
    pub(super) listing: Option<ProjectsReply>,
    /// Why the last listing did not arrive.
    pub(super) listing_error: Option<String>,
    pub(super) asked: Option<Instant>,
    pub(super) asking: bool,
    /// The Overview's project search.
    pub(super) search: String,
}

impl ProjectsUi {
    /// The id of an editor-made tab or group, minted the first time.
    fn mint(&mut self, slot: ViewSlot) -> u64 {
        if let Some(id) = self.ids.get(&slot) {
            return *id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.ids.insert(slot.clone(), id);
        self.slots.insert(id, slot);
        id
    }

    pub(super) fn id(&self, slot: &ViewSlot) -> Option<u64> {
        self.ids.get(slot).copied()
    }

    pub(super) fn slot(&self, id: u64) -> Option<&ViewSlot> {
        self.slots.get(&id)
    }

    /// The group of `origin`'s project, once it was composed.
    pub(super) fn group_id(&self, origin: &str) -> Option<GroupId> {
        self.id(&ViewSlot::Board(origin.to_owned())).map(GroupId)
    }

    /// The section with the projects the user opened in an earlier run.
    pub(super) fn with_open(open: impl IntoIterator<Item = String>) -> ProjectsUi {
        ProjectsUi {
            open: open.into_iter().collect(),
            ..ProjectsUi::default()
        }
    }
}

/// One project the Overview lists: the runner's listing, and every
/// repository of a connected CI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub origin: String,
    pub name: String,
    /// A repository of a connected CI.
    pub ci: bool,
    /// Its work checkout exists on the local runner's host.
    pub checkout: bool,
    pub default_branch: String,
}

impl App {
    /// What a runner tab of a project group shows, by its first terminal
    /// and what the runner and the CIs say about it.
    fn tab_kind(&self, key: &RunnerKey, tab: TabId) -> TabKind {
        let Some(terminal) = self
            .workspace
            .section(key)
            .and_then(|s| s.snapshot.tabs().find(|t| t.id == tab))
            .and_then(|t| t.root.terminals().next())
        else {
            return TabKind::Other;
        };
        let entry = self
            .runtime
            .activity
            .get(key)
            .and_then(|entries| entries.iter().find(|e| e.terminal == terminal.0));
        match entry.map(|e| e.kind) {
            Some(ActivityKind::Shell) => TabKind::Work,
            Some(ActivityKind::Session) => {
                let master = self.ci.values().any(|client| {
                    client.sessions().any(|s| {
                        s.terminal == Some(terminal.0)
                            && s.kind == zeughaus_link::SessionKind::Master
                            && client.runner_key(&s.runner).as_ref() == Some(key)
                    })
                });
                if master {
                    TabKind::Master
                } else {
                    TabKind::Plan
                }
            }
            _ => TabKind::Other,
        }
    }

    /// A project's name: its repository's in a CI that builds it, else the
    /// last segment of its origin.
    pub(super) fn project_name(&self, origin: &str) -> String {
        self.ci
            .values()
            .find_map(|client| client.repo_docs_by_origin(origin))
            .map(|repo| repo.repo.clone())
            .unwrap_or_else(|| origin.rsplit('/').next().unwrap_or(origin).to_owned())
    }

    /// The branch a missing work checkout is cloned at.
    fn default_branch(&self, origin: &str) -> String {
        self.listed_projects()
            .into_iter()
            .find(|p| p.origin == origin)
            .map_or_else(|| "main".to_owned(), |p| p.default_branch)
    }

    /// Every project the Overview lists: the local runner's listing, then
    /// the repositories of the CIs it does not list, by name.
    pub(super) fn listed_projects(&self) -> Vec<Listed> {
        let ci_repos: Vec<&zeughaus_link::RepoDocsView> = self
            .ci
            .values()
            .flat_map(|client| client.repo_docs())
            .collect();
        let mut listed: Vec<Listed> = self
            .projects
            .listing
            .iter()
            .flat_map(|reply| &reply.projects)
            .map(|p: &ProjectView| Listed {
                origin: p.origin.clone(),
                name: p.name.clone(),
                ci: ci_repos.iter().any(|r| r.origin == p.origin),
                checkout: p.checkout,
                default_branch: p.default_branch.clone(),
            })
            .collect();
        for repo in ci_repos {
            if listed.iter().any(|p| p.origin == repo.origin) {
                continue;
            }
            listed.push(Listed {
                origin: repo.origin.clone(),
                name: repo.repo.clone(),
                ci: true,
                checkout: false,
                default_branch: repo.default_branch.clone(),
            });
        }
        listed.sort_by(|a, b| (&a.name, &a.origin).cmp(&(&b.name, &b.origin)));
        listed
    }

    /// Composes the Projects section from the workspace, mints the ids of
    /// its new tabs and groups, and starts a new group collapsed while the
    /// bar is on top.
    pub(super) fn sync_projects(&mut self) {
        let runners: Vec<RunnerGroups<'_>> = self
            .workspace
            .sections()
            .iter()
            .filter(|section| !section.synthetic())
            .map(|section| RunnerGroups {
                key: section.key.clone(),
                local: self.local_runner.as_ref() == Some(&section.key),
                groups: section
                    .snapshot
                    .groups()
                    .filter(|group| group.project.is_some())
                    .collect(),
            })
            .collect();
        let groups = compose(&ComposeInput {
            open: &self.projects.open,
            dismissed: &self.projects.dismissed,
            runners,
            kind_of: |key: &RunnerKey, tab: TabId| self.tab_kind(key, tab),
            names: |origin: &str| self.project_name(origin),
            views: &self.projects.views,
        });
        let top = self.workspace.placement == iced_tabs::Placement::Top;
        for group in &groups {
            let board = self.projects.mint(ViewSlot::Board(group.origin.clone()));
            if self.projects.seen.insert(group.origin.clone()) && top {
                self.workspace
                    .collapsed
                    .insert(CollapseKey::Group(RunnerKey::projects(), GroupId(board)));
            }
            for tab in &group.tabs {
                match tab {
                    GroupTab::Start => {
                        self.projects.mint(ViewSlot::Start(group.origin.clone()));
                    }
                    GroupTab::View(view) => {
                        self.projects
                            .mint(ViewSlot::View(group.origin.clone(), view.clone()));
                    }
                    GroupTab::Board | GroupTab::Runner(_) => {}
                }
            }
        }
        self.projects.groups = groups;
    }

    /// The tab the bar marks for what the page shows: a Board or a channel
    /// view of a project's group.
    pub(super) fn shown_project_tab(&self) -> Option<TabRef> {
        if !self.page.open {
            return None;
        }
        let slot = match &self.page.view {
            PageView::Board(origin) => ViewSlot::Board(origin.clone()),
            PageView::Channel(ci, view) => {
                let key = ViewKey::Channel(ci.clone(), *view);
                let (origin, _) = self.projects.views.iter().find(|(_, v)| *v == key)?;
                ViewSlot::View(origin.clone(), key)
            }
            PageView::Overview => return None,
        };
        Some(TabRef {
            runner: RunnerKey::projects(),
            tab: TabId(self.projects.id(&slot)?),
        })
    }

    /// Every tab of the bar in the order it is drawn, without the buttons
    /// that start a work shell.
    fn bar_order(&self) -> Vec<BarTab> {
        let mut order = Vec::new();
        for section in self.workspace.sections() {
            for item in &section.snapshot.items {
                let tabs: Vec<&zeughaus_mux::TabSnapshot> = match item {
                    zeughaus_mux::WorkspaceItem::Tab(tab) => vec![tab],
                    zeughaus_mux::WorkspaceItem::Group(group) if group.project.is_none() => {
                        group.tabs.iter().collect()
                    }
                    zeughaus_mux::WorkspaceItem::Group(_) => Vec::new(),
                };
                order.extend(tabs.into_iter().map(|tab| {
                    BarTab::Runner(TabRef {
                        runner: section.key.clone(),
                        tab: tab.id,
                    })
                }));
            }
        }
        for group in &self.projects.groups {
            for tab in &group.tabs {
                let slot = match tab {
                    GroupTab::Board => ViewSlot::Board(group.origin.clone()),
                    GroupTab::View(view) => ViewSlot::View(group.origin.clone(), view.clone()),
                    GroupTab::Runner(tab) => {
                        order.push(BarTab::Runner(tab.clone()));
                        continue;
                    }
                    GroupTab::Start => continue,
                };
                if let Some(id) = self.projects.id(&slot) {
                    order.push(BarTab::Project(TabId(id)));
                }
            }
        }
        order
    }

    /// Ctrl+PageDown and Ctrl+PageUp: the tab `step` places from the one in
    /// front, in the bar's order.
    pub(super) fn cycle_bar(&mut self, step: isize) -> Task<Message> {
        let order = self.bar_order();
        let front = match self.shown_project_tab() {
            Some(tab) => Some(BarTab::Project(tab.tab)),
            None if self.page.open => None,
            None => self.workspace.active_tab().cloned().map(BarTab::Runner),
        };
        let at = front.and_then(|front| order.iter().position(|tab| *tab == front));
        let Some(next) = crate::workspace::cycle_index(order.len(), at, step) else {
            return Task::none();
        };
        match order[next].clone() {
            BarTab::Runner(tab) => {
                self.page.open = false;
                self.workspace.activate(tab);
                Task::none()
            }
            BarTab::Project(tab) => self.project_tab(tab),
        }
    }

    /// A press on a tab of the Projects section the editor made.
    pub(super) fn project_tab(&mut self, tab: TabId) -> Task<Message> {
        match self.projects.slot(tab.0).cloned() {
            Some(ViewSlot::Board(origin)) => {
                self.apply_page(Message::PageShow(PageView::Board(origin)))
            }
            Some(ViewSlot::Start(origin)) => self.open_project(origin),
            Some(ViewSlot::View(_, ViewKey::Channel(ci, view))) => {
                self.apply_page(Message::PageShow(PageView::Channel(ci, view)))
            }
            None => Task::none(),
        }
    }

    /// Closes a view of a project's group, and the page when it shows it.
    pub(super) fn close_project_tab(&mut self, tab: TabId) {
        let Some(ViewSlot::View(origin, view)) = self.projects.slot(tab.0).cloned() else {
            return;
        };
        if self
            .shown_project_tab()
            .is_some_and(|shown| shown.tab == tab)
        {
            self.page.open = false;
        }
        self.projects
            .views
            .retain(|(o, v)| !(*o == origin && *v == view));
    }

    /// Shows `origin`'s group in the bar from now on, saved.
    pub(super) fn keep_project(&mut self, origin: &str) {
        self.projects.dismissed.remove(origin);
        if self.projects.open.insert(origin.to_owned()) {
            self.save_prefs();
        }
    }

    /// `Project / Open` and the `~/work/<name>` button: the project's group,
    /// and a shell in its work checkout on this machine's runner.
    pub(super) fn open_project(&mut self, origin: String) -> Task<Message> {
        self.keep_project(&origin);
        let attached = self
            .local_runner
            .clone()
            .filter(|key| self.mux.get(key).is_some_and(|mux| mux.attached));
        let Some(key) = attached else {
            let reason = if self.local_runner.is_some() {
                "the local runner is not attached"
            } else {
                "no local runner"
            };
            self.notices.raise(
                None,
                Severity::Warning,
                "Cannot open a shell",
                reason,
                false,
                None,
                std::time::SystemTime::now(),
            );
            return Task::none();
        };
        let branch = self.default_branch(&origin);
        // The shell comes to the front once the runner shows it; a view over
        // it would hide it.
        self.page.open = false;
        self.workspace.expect_new_tab(&key);
        self.send_topology(&key, TopologyCommand::OpenProject { origin, branch });
        Task::none()
    }

    /// The Overview's `Open`: the project's group and its Board, no shell.
    pub(super) fn show_project(&mut self, origin: String) -> Task<Message> {
        self.keep_project(&origin);
        self.sync_projects();
        self.apply_page(Message::PageShow(PageView::Board(origin)))
    }

    /// The close button of a project's group: it goes until it is opened
    /// again or a runner tab of it shows up that is not there now.
    pub(super) fn dismiss_project(&mut self, origin: String) {
        let tabs: BTreeSet<(RunnerKey, TabId)> = self
            .projects
            .groups
            .iter()
            .filter(|group| group.origin == origin)
            .flat_map(|group| &group.tabs)
            .filter_map(|tab| match tab {
                GroupTab::Runner(tab) => Some((tab.runner.clone(), tab.tab)),
                _ => None,
            })
            .collect();
        if let PageView::Board(shown) = &self.page.view
            && *shown == origin
        {
            self.page.open = false;
        }
        self.projects.views.retain(|(o, _)| *o != origin);
        // The tab in front goes with its group: another runner tab comes
        // to the front.
        let hidden = |tab: &TabRef| tabs.contains(&(tab.runner.clone(), tab.tab));
        if self.workspace.active_tab().is_some_and(hidden) {
            let next = self.bar_order().into_iter().find_map(|tab| match tab {
                BarTab::Runner(tab) if !hidden(&tab) => Some(tab),
                _ => None,
            });
            if let Some(tab) = next {
                self.workspace.activate(tab);
            }
        }
        self.projects.dismissed.insert(origin.clone(), tabs);
        if self.projects.open.remove(&origin) {
            self.save_prefs();
        }
    }

    /// A channel badge of a project's group: the channel's view as a tab of
    /// the group, in front.
    pub(super) fn open_channel_view(&mut self, origin: String, channel: String) -> Task<Message> {
        let found = self.ci.iter().find_map(|(ci, client)| {
            let repo = client.repo_docs_by_origin(&origin)?;
            Some((ci.clone(), client.channel_view(&repo.repo, &channel)?))
        });
        let Some((ci, view)) = found else {
            return Task::none();
        };
        self.apply_page(Message::PageShow(PageView::Channel(ci, view)))
    }

    /// Makes a channel view a tab of its project's group the first time it
    /// is shown.
    pub(super) fn adopt_channel_view(&mut self, ci: &RunnerKey, view: u64) {
        let key = ViewKey::Channel(ci.clone(), view);
        if self.projects.views.iter().any(|(_, v)| *v == key) {
            return;
        }
        let Some(origin) = self
            .ci
            .get(ci)
            .and_then(|client| client.origin_of_view(view))
        else {
            return;
        };
        self.keep_project(&origin);
        self.projects.views.push((origin, key));
    }

    /// Asks the local runner which projects it can open, when it was not
    /// asked within `ASK_AGAIN` or `refresh` says to ask the forges again.
    pub(super) fn ask_projects(&mut self, refresh: bool) -> Task<Message> {
        let recent = self
            .projects
            .asked
            .is_some_and(|asked| asked.elapsed() < ASK_AGAIN);
        if self.projects.asking || (recent && !refresh) {
            return Task::none();
        }
        let Some(endpoint) = self
            .local_runner
            .as_ref()
            .and_then(|key| self.runtime.links.get(key))
            .map(|link| link.endpoint.clone())
        else {
            return Task::none();
        };
        self.projects.asking = true;
        self.projects.asked = Some(Instant::now());
        Task::perform(
            crate::feed::projects(endpoint, refresh),
            Message::ProjectsArrived,
        )
    }

    pub(super) fn projects_arrived(&mut self, result: Result<ProjectsReply, String>) {
        self.projects.asking = false;
        match result {
            Ok(reply) => {
                self.projects.listing = Some(reply);
                self.projects.listing_error = None;
            }
            Err(e) => self.projects.listing_error = Some(e),
        }
        self.rebuild_palette();
    }

    /// The terminal a collapsed group's runner tab waits on the user with:
    /// the badge the group header carries for it.
    pub(super) fn waiting_agent(&self, tab: &TabRef) -> Option<(TerminalId, char, iced::Color)> {
        let snapshot = self
            .workspace
            .section(&tab.runner)?
            .snapshot
            .tabs()
            .find(|t| t.id == tab.tab)?;
        let (_, entry) = self.runtime.agents().find(|(key, entry)| {
            *key == &tab.runner && snapshot.root.terminals().any(|t| t.0 == entry.terminal)
        })?;
        let agent = entry.agent.as_ref().filter(|a| a.needs_attention())?;
        Some((
            TerminalId(entry.terminal),
            super::agents::role_glyph(agent.role),
            super::agents::agent_color(agent.state, &self.theme),
        ))
    }

    /// The Projects section as the tab tree draws it, with what each
    /// group's close button and badges mean: the group's origin and one
    /// message per badge.
    pub(super) fn projects_section(&self) -> ProjectsSection<'_> {
        let projects = RunnerKey::projects();
        let mut origins = HashMap::new();
        let mut badge_actions = HashMap::new();
        let mut items = Vec::new();
        for group in &self.projects.groups {
            let Some(gid) = self.projects.group_id(&group.origin) else {
                continue;
            };
            let collapsed = self
                .workspace
                .collapsed
                .contains(&CollapseKey::Group(projects.clone(), gid));
            let mut badges = Vec::new();
            let mut actions = Vec::new();
            for badge in self.channel_badges(&group.origin) {
                actions.push(Message::OpenChannelView(
                    group.origin.clone(),
                    badge.channel.clone(),
                ));
                badges.push(iced_tabs::Badge {
                    glyph: badge.glyph,
                    color: badge.color,
                    tooltip: badge.tooltip.into(),
                });
            }
            let mut tabs = Vec::new();
            for tab in &group.tabs {
                let synthetic = |slot: ViewSlot| {
                    self.projects.id(&slot).map(|id| TabRef {
                        runner: projects.clone(),
                        tab: TabId(id),
                    })
                };
                let entry = match tab {
                    GroupTab::Board => synthetic(ViewSlot::Board(group.origin.clone()))
                        .map(|id| iced_tabs::Tab::new(id, "Board").closable(false)),
                    GroupTab::Start => synthetic(ViewSlot::Start(group.origin.clone())).map(|id| {
                        iced_tabs::Tab::new(id, format!("~/work/{}", group.name))
                            .host('\u{ea60}', "Open a shell in the work checkout")
                            .closable(false)
                    }),
                    GroupTab::View(view) => {
                        let ViewKey::Channel(ci, channel_view) = view;
                        let label = self
                            .ci
                            .get(ci)
                            .and_then(|client| client.channel_name(*channel_view))
                            .unwrap_or("channel");
                        synthetic(ViewSlot::View(group.origin.clone(), view.clone()))
                            .map(|id| iced_tabs::Tab::new(id, label))
                    }
                    GroupTab::Runner(tab) => {
                        if collapsed && let Some((terminal, glyph, color)) = self.waiting_agent(tab)
                        {
                            actions.push(Message::RevealTerminal(tab.runner.clone(), terminal));
                            badges.push(iced_tabs::Badge {
                                glyph,
                                color,
                                tooltip: "Waits for you".into(),
                            });
                        }
                        self.workspace
                            .section(&tab.runner)
                            .and_then(|s| s.snapshot.tabs().find(|t| t.id == tab.tab))
                            .map(|snapshot| {
                                let entry = self.tab_entry(&tab.runner, snapshot);
                                match self.host_of(&tab.runner) {
                                    Some((glyph, name)) => entry.host(glyph, name),
                                    None => entry,
                                }
                            })
                    }
                };
                tabs.extend(entry);
            }
            origins.insert(gid, group.origin.clone());
            badge_actions.insert(gid, actions);
            let [r, g, b, a] = zeughaus_mux::project_color(&group.name);
            items.push(iced_tabs::Item::Group(iced_tabs::Group {
                id: gid,
                label: group.name.as_str().into(),
                color: iced::Color::from_rgba8(r, g, b, f32::from(a) / 255.0),
                locked: true,
                collapsed,
                tabs,
                editor: None,
                host: None,
                badges,
                closable: true,
            }));
        }
        ProjectsSection {
            section: iced_tabs::Section {
                id: projects,
                label: "Projects".into(),
                header: true,
                collapsed: false,
                controls: vec![
                    iced_tabs::Control::new("open-project", '\u{ea60}'.to_string())
                        .font(crate::font::UI)
                        .tooltip("Open a project"),
                ],
                items,
            },
            origins,
            badges: badge_actions,
        }
    }
}

/// What [`App::projects_section`] builds.
pub(super) struct ProjectsSection<'a> {
    pub section: iced_tabs::Section<'a, TabRef, GroupId, RunnerKey, Message, zeughaus_theme::Theme>,
    /// Each group's project.
    pub origins: HashMap<GroupId, String>,
    /// What each group's badges do, in their order.
    pub badges: HashMap<GroupId, Vec<Message>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_mux::{PaneId, PaneNode, SurfaceRef, TabSnapshot};

    fn key(name: &str) -> RunnerKey {
        RunnerKey::new(name)
    }

    fn tab(id: u64) -> TabSnapshot {
        TabSnapshot {
            id: TabId(id),
            title: format!("t{id}"),
            accent_rgba: None,
            root: PaneNode::Leaf {
                pane_id: PaneId(id),
                surface: SurfaceRef::Terminal(TerminalId(id)),
            },
        }
    }

    fn group(origin: &str, tabs: &[u64]) -> GroupSnapshot {
        GroupSnapshot {
            id: GroupId(1),
            name: origin.rsplit('/').next().unwrap().to_owned(),
            color_rgba: [0; 4],
            locked: true,
            tabs: tabs.iter().map(|id| tab(*id)).collect(),
            project: Some(origin.to_owned()),
        }
    }

    /// Tabs 1..=9 are work shells, 10..=19 masters, 20..=29 plan sessions,
    /// the rest anything else.
    fn kind(_: &RunnerKey, tab: TabId) -> TabKind {
        match tab.0 {
            1..=9 => TabKind::Work,
            10..=19 => TabKind::Master,
            20..=29 => TabKind::Plan,
            _ => TabKind::Other,
        }
    }

    fn name(origin: &str) -> String {
        origin.rsplit('/').next().unwrap().to_owned()
    }

    fn runner<'a>(key_name: &str, local: bool, groups: Vec<&'a GroupSnapshot>) -> RunnerGroups<'a> {
        RunnerGroups {
            key: key(key_name),
            local,
            groups,
        }
    }

    fn at(runner: &str, id: u64) -> GroupTab {
        GroupTab::Runner(TabRef {
            runner: key(runner),
            tab: TabId(id),
        })
    }

    #[test]
    fn a_group_orders_board_work_masters_plans_rest_and_views_local_runner_first() {
        let remote = group("h/o/zeughaus", &[30, 20, 10, 1]);
        let local = group("h/o/zeughaus", &[11]);
        let open = BTreeSet::new();
        let dismissed = HashMap::new();
        let view = ViewKey::Channel(key("ci"), 4);
        let views = vec![("h/o/zeughaus".to_owned(), view.clone())];
        let groups = compose(&ComposeInput {
            open: &open,
            dismissed: &dismissed,
            runners: vec![
                runner("remote", false, vec![&remote]),
                runner("local", true, vec![&local]),
            ],
            kind_of: kind,
            names: name,
            views: &views,
        });
        assert_eq!(
            groups,
            [ProjectGroup {
                origin: "h/o/zeughaus".to_owned(),
                name: "zeughaus".to_owned(),
                tabs: vec![
                    GroupTab::Board,
                    at("remote", 1),
                    at("local", 11),
                    at("remote", 10),
                    at("remote", 20),
                    at("remote", 30),
                    GroupTab::View(view),
                ],
            }]
        );
    }

    #[test]
    fn an_open_project_without_tabs_offers_its_board_and_the_start_button() {
        let open: BTreeSet<String> = ["h/o/weida".to_owned(), "h/o/demo".to_owned()].into();
        let dismissed = HashMap::new();
        let master = group("h/o/weida", &[10]);
        let groups = compose(&ComposeInput {
            open: &open,
            dismissed: &dismissed,
            runners: vec![runner("local", true, vec![&master])],
            kind_of: kind,
            names: name,
            views: &[],
        });
        let tabs: Vec<(&str, &[GroupTab])> = groups
            .iter()
            .map(|g| (g.name.as_str(), g.tabs.as_slice()))
            .collect();
        assert_eq!(
            tabs,
            [
                ("demo", &[GroupTab::Board, GroupTab::Start][..]),
                (
                    "weida",
                    &[GroupTab::Board, GroupTab::Start, at("local", 10)][..]
                ),
            ]
        );
    }

    #[test]
    fn a_dismissed_project_returns_with_a_tab_it_did_not_have() {
        let open = BTreeSet::new();
        let dismissed: HashMap<String, BTreeSet<(RunnerKey, TabId)>> =
            [("h/o/weida".to_owned(), [(key("local"), TabId(10))].into())].into();
        let before = group("h/o/weida", &[10]);
        let compose_with = |g: &GroupSnapshot| {
            compose(&ComposeInput {
                open: &open,
                dismissed: &dismissed,
                runners: vec![runner("local", true, vec![g])],
                kind_of: kind,
                names: name,
                views: &[],
            })
        };
        assert!(compose_with(&before).is_empty());
        let after = group("h/o/weida", &[10, 21]);
        assert_eq!(compose_with(&after).len(), 1);
    }
}
