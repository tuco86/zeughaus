//! The CI sections: what this editor shows of a runner's CI.
//!
//! A runner that runs CI gets a section of its own in the tab bar, beside its
//! workspace: a Runners tab, and under a group per repository one tab per
//! channel. The editor holds only what it was told over `/ci` and computes no
//! status; when the runner announces that a pipeline changed it asks for that
//! pipeline again. The sections are the editor's own, built the way the
//! `local` section is built from the document: a snapshot whose ids this
//! module mints and no runner knows.
//!
//! A channel tab lists the channel's pipelines, newest on top, in a plain
//! scrollable: per pipeline its trigger, commit and status over a drawing of
//! its jobs. The jobs are cards laid out by what they need, joined by cables
//! drawn with the node graph's renderer; nothing in it pans, zooms or moves.
//! Pressing a job asks the runner for a transcript: a read-only terminal of
//! the runner's own mux that replays the job's recorded output, live while
//! the job runs, and which the tab then shows beside the list like any other
//! terminal.
//!
//! Native only: the browser editor reaches no runner.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

use iced::widget::text::Wrapping;
use iced::widget::{
    Space, button, column, container, pin, responsive, row, rule, scrollable, stack, text, tooltip,
};
use iced::{Alignment, Color, Element, Length, Padding, Point, Size, Task};
use iced_nodegraph_sdf::Shape;
use zeughaus_core::NodeId;
use zeughaus_link::{
    CiOverview, CiReply, CiRequest, JobView, MachineJob, MachineView, PipelineView,
};
use zeughaus_mux::{
    Axis, GroupId, GroupSnapshot, PaneId, PaneNode, SplitId, TabId, TabSnapshot, TerminalId,
    WorkspaceItem, WorkspaceSnapshot,
};
use zeughaus_theme::Theme;

use self::cables::{Cables, cable};
use super::layout::auto_cells;
use super::runtime::LinkKind;
use super::{App, unavailable};
use crate::feed;
use crate::message::{CiMsg, Message};
use crate::transport::Endpoint;
use crate::workspace::{self, RunnerKey, Surface, TabRef};

mod cables;

/// The id of the Runners view, its tab and its pane. Every other id of a
/// section comes from its client's counter, which starts above this.
const RUNNERS: u64 = 1;

/// Pipelines asked for at a time: a channel's first page, and each page
/// "Load older" adds.
const PAGE: u32 = 20;

/// Set on a view's id for the split that holds its transcript and, with the
/// next bit, for the pane of that transcript. Views, tabs and groups are
/// numbered from a counter and never get near either bit.
const SPLIT_BIT: u64 = 1 << 62;
const TRANSCRIPT_BIT: u64 = 1 << 63;

/// A repository's group has no colour of its own.
const GROUP_GREY: [u8; 4] = [0x80, 0x80, 0x80, 0xff];

/// The size of a job's card. It is a fixed box, so a pipeline's drawing can
/// be sized without measuring what is in it.
const JOB_WIDTH: f32 = 212.0;
const JOB_HEIGHT: f32 = 52.0;
/// Space between two columns of cards: the room a cable bends in.
const COLUMN_GAP: f32 = 56.0;
/// Space between two cards of one column.
const ROW_GAP: f32 = 12.0;
/// Characters of a job's note its card shows.
const NOTE_CHARS: usize = 60;
/// Characters of a card's second line: what fits beside the glyph.
const CARD_CHARS: usize = 26;
/// Lines of a failed job's log its pipeline shows.
const EXCERPT_LINES: usize = 14;
/// Lines before the cause line shown with it: the test or step it is in.
const EXCERPT_CONTEXT: usize = 2;

/// The spacing scale of the CI views.
const SP_1: f32 = 4.0;
const SP_2: f32 = 8.0;
const SP_3: f32 = 12.0;
const SP_4: f32 = 16.0;
/// Text sizes: secondary lines, and everything else.
const TEXT_SM: f32 = 12.0;
const TEXT_MD: f32 = 14.0;
/// Status glyphs, and the dot of a machine's state.
const ICON_SIZE: f32 = 16.0;
const DOT_SIZE: f32 = 10.0;
/// The column a row's status glyph stands in, so text after it aligns.
const ICON_COLUMN: f32 = 20.0;
/// The column of a pipeline row's duration and age.
const TIME_COLUMN: f32 = 72.0;
/// The height of a view's toolbar, the same with and without buttons.
const TOOLBAR_HEIGHT: f32 = 40.0;
/// Buttons reach the 24 px pointer target floor.
const BUTTON_HEIGHT: f32 = 26.0;
const RADIUS: f32 = 6.0;
/// Column widths of the Runners table.
const MACHINE_WIDTH: f32 = 160.0;
const STATE_WIDTH: f32 = 150.0;

/// This editor's view of one runner's CI.
pub(super) struct CiClient {
    /// Where the runner serves. A different one is a different runner process,
    /// whose terminals and pipelines these views do not describe.
    endpoint: Endpoint,
    /// The runner's label, which the section's is made of.
    label: String,
    overview: Option<CiOverview>,
    /// Whether an overview is on its way. One at a time: the clock and every
    /// change event ask, and answering all of them would be a storm.
    overview_pending: bool,
    /// A change arrived while one was on its way, which may not have seen it.
    overview_again: bool,
    /// Why the last overview did not arrive, while none has.
    error: Option<String>,
    /// The next id for a view or a group. Ids are never reused in a session,
    /// so a message for a view that is gone cannot reach another one.
    next_id: u64,
    repos: BTreeMap<String, Repo>,
    views: BTreeMap<u64, ChannelState>,
    /// Pipelines being fetched after a change event, and whether another
    /// change arrived meanwhile: a reply older than the last event would
    /// otherwise be the last word.
    fetching: HashMap<(String, u64), bool>,
}

/// A repository: its group in the tab bar and the views of its channels.
struct Repo {
    group: u64,
    /// Channel name to view id.
    channels: BTreeMap<String, u64>,
}

/// One channel's tab: the pipelines loaded so far and what is open beside
/// them. The tab is titled with the channel alone: its group names the
/// repository.
struct ChannelState {
    repo: String,
    channel: String,
    /// Newest first.
    pipelines: Vec<Shown>,
    /// Whether the first page has arrived. A change that comes before it is
    /// part of it.
    loaded: bool,
    /// Whether the newest page is out of date because asking for it failed:
    /// the next overview asks again.
    stale: bool,
    loading: bool,
    /// Whether the runner has nothing older than the last pipeline loaded.
    exhausted: bool,
    error: Option<String>,
    transcript: Option<Transcript>,
    /// Pipelines the user opened or closed against their default: the
    /// newest one and the running ones are open, the rest closed.
    toggled: HashSet<u64>,
    /// Pipelines with a rerun or cancel on its way, whose buttons wait.
    acting: HashSet<u64>,
    /// What the last rerun or cancel came to.
    notice: Option<Notice>,
}

/// The answer to a rerun or a cancel, said in the toolbar until the next.
struct Notice {
    text: String,
    failed: bool,
}

/// The job whose output a view shows beside its list.
struct Transcript {
    number: u64,
    job: String,
    /// The runner's terminal id, once it has answered.
    terminal: Option<u64>,
    /// Why it did not.
    error: Option<String>,
}

/// A pipeline as the runner reported it, with what is drawn from it worked
/// out once rather than on every frame.
struct Shown {
    pipeline: PipelineView,
    jobs: JobGraph,
}

impl Shown {
    fn new(pipeline: PipelineView) -> Shown {
        Shown {
            jobs: JobGraph::of(&pipeline),
            pipeline,
        }
    }
}

/// The drawing of a pipeline's jobs: where each card goes and the cables
/// between them, in the drawing's own coordinates.
struct JobGraph {
    size: Size,
    /// Top left corner of each job's card, in the pipeline's order.
    cards: Vec<Point>,
    /// One per need, from the right edge of the needed job's card to the
    /// left edge of the needing one's.
    cables: Vec<Shape>,
    /// Where a cable meets a card, once per card edge.
    ports: Vec<[f32; 2]>,
}

/// `text` on one line, cut after `limit` characters.
fn flatten(text: &str, limit: usize) -> String {
    text.chars()
        .take(limit)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

impl JobGraph {
    fn of(pipeline: &PipelineView) -> JobGraph {
        let jobs = &pipeline.jobs;
        let index: HashMap<&str, usize> = jobs
            .iter()
            .enumerate()
            .map(|(at, job)| (job.name.as_str(), at))
            .collect();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (to, job) in jobs.iter().enumerate() {
            for need in &job.needs {
                if let Some(&from) = index.get(need.as_str())
                    && from != to
                    && !edges.contains(&(from, to))
                {
                    edges.push((from, to));
                }
            }
        }

        let nodes: Vec<NodeId> = (0..jobs.len() as u64).map(NodeId).collect();
        let wires: Vec<(NodeId, NodeId)> = edges
            .iter()
            .map(|&(from, to)| (NodeId(from as u64), NodeId(to as u64)))
            .collect();
        let mut cards = vec![Point::ORIGIN; jobs.len()];
        for (node, (column, row)) in auto_cells(&nodes, &wires) {
            if let Some(card) = cards.get_mut(node.0 as usize) {
                *card = Point::new(
                    column as f32 * (JOB_WIDTH + COLUMN_GAP),
                    row as f32 * (JOB_HEIGHT + ROW_GAP),
                );
            }
        }

        let output = |at: usize| [cards[at].x + JOB_WIDTH, cards[at].y + JOB_HEIGHT * 0.5];
        let input = |at: usize| [cards[at].x, cards[at].y + JOB_HEIGHT * 0.5];
        let cables = edges
            .iter()
            .map(|&(from, to)| cable(output(from), input(to)))
            .collect();
        let mut outputs = vec![false; jobs.len()];
        let mut inputs = vec![false; jobs.len()];
        for &(from, to) in &edges {
            outputs[from] = true;
            inputs[to] = true;
        }
        let ports = (0..jobs.len())
            .filter(|&at| outputs[at])
            .map(output)
            .chain((0..jobs.len()).filter(|&at| inputs[at]).map(input))
            .collect();

        let size = if cards.is_empty() {
            Size::ZERO
        } else {
            let right = cards.iter().map(|card| card.x).fold(0.0, f32::max);
            let bottom = cards.iter().map(|card| card.y).fold(0.0, f32::max);
            Size::new(right + JOB_WIDTH, bottom + JOB_HEIGHT)
        };
        JobGraph {
            size,
            cards,
            cables,
            ports,
        }
    }
}

impl ChannelState {
    fn new(repo: &str, channel: &str) -> ChannelState {
        ChannelState {
            repo: repo.to_owned(),
            channel: channel.to_owned(),
            pipelines: Vec::new(),
            loaded: false,
            loading: false,
            stale: false,
            exhausted: false,
            error: None,
            transcript: None,
            toggled: HashSet::new(),
            acting: HashSet::new(),
            notice: None,
        }
    }

    /// Whether the pipeline at `at` in the list shows its jobs: the newest
    /// and every one still going do, unless the user closed them, and the
    /// user opens any other.
    fn is_open(&self, at: usize, pipeline: &PipelineView) -> bool {
        let open = at == 0 || (pipeline.finished.is_none() && pipeline.status == "running");
        open != self.toggled.contains(&pipeline.number)
    }

    /// Opens a closed pipeline and closes an open one.
    fn toggle(&mut self, number: u64) {
        if !self.toggled.remove(&number) {
            self.toggled.insert(number);
        }
    }

    /// What a rerun or a cancel came to, for the toolbar.
    fn acted(&mut self, number: u64, cancel: bool, result: Result<(), String>) {
        self.acting.remove(&number);
        self.notice = Some(match (result, cancel) {
            (Ok(()), true) => Notice {
                text: format!("Cancelling #{number}"),
                failed: false,
            },
            (Ok(()), false) => Notice {
                text: format!("Queued a rerun of #{number}"),
                failed: false,
            },
            (Err(e), cancel) => Notice {
                text: format!(
                    "{} #{number} refused: {e}",
                    if cancel { "Cancelling" } else { "Rerunning" }
                ),
                failed: true,
            },
        });
    }

    /// What a pipeline's change says about this channel: the pipeline is in
    /// it, now as `found`, or it is not (it moved to another channel, or the
    /// runner dropped it).
    fn changed(&mut self, number: u64, found: Option<&PipelineView>) {
        // Before the first page there is nothing to keep current, and the page
        // will have the change.
        if !self.loaded {
            return;
        }
        match found {
            Some(pipeline) if pipeline.channel == self.channel => {
                self.upsert(pipeline.clone());
            }
            _ => self
                .pipelines
                .retain(|shown| shown.pipeline.number != number),
        }
    }

    /// Whether a pipeline or a job of the channel is running with a start
    /// to count from: what a ticking clock has to redraw.
    fn ticking(&self) -> bool {
        self.pipelines.iter().any(|shown| {
            let pipeline = &shown.pipeline;
            (pipeline.finished.is_none() && matches!(Tone::of(&pipeline.status), Tone::Active))
                || pipeline.jobs.iter().any(|job| {
                    job.started.is_some()
                        && job.finished.is_none()
                        && matches!(Tone::of(&job.status), Tone::Active)
                })
        })
    }

    fn upsert(&mut self, pipeline: PipelineView) {
        let number = pipeline.number;
        // Newest first, so a number sorts after the ones above it.
        match self
            .pipelines
            .binary_search_by(|shown| number.cmp(&shown.pipeline.number))
        {
            Ok(at) => self.pipelines[at] = Shown::new(pipeline),
            // Older than everything shown while older ones may exist: the page
            // "Load older" fetches has it, and a gap would not.
            Err(at)
                if at == self.pipelines.len() && !self.pipelines.is_empty() && !self.exhausted => {}
            Err(at) => self.pipelines.insert(at, Shown::new(pipeline)),
        }
    }

    /// The tab's pane tree: the graph, and the transcript beside it once the
    /// runner has opened one.
    fn root(&self, view: u64) -> PaneNode {
        let graph = PaneNode::Leaf {
            pane_id: PaneId(view),
            surface: Surface::Ci(view),
        };
        match self.transcript.as_ref().and_then(|t| t.terminal) {
            Some(terminal) => PaneNode::Split {
                id: SplitId(view | SPLIT_BIT),
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(graph),
                second: Box::new(PaneNode::Leaf {
                    pane_id: PaneId(view | TRANSCRIPT_BIT),
                    surface: Surface::Terminal(TerminalId(terminal)),
                }),
            },
            None => graph,
        }
    }
}

impl CiClient {
    fn new(endpoint: Endpoint, label: String) -> CiClient {
        CiClient {
            endpoint,
            label,
            overview: None,
            overview_pending: false,
            overview_again: false,
            error: None,
            next_id: RUNNERS + 1,
            repos: BTreeMap::new(),
            views: BTreeMap::new(),
            fetching: HashMap::new(),
        }
    }

    fn mint(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The terminals of the transcripts the runner has opened for this
    /// client's views: what the runner's mux has to stream to this editor
    /// besides the terminals of its workspace.
    pub(super) fn terminals(&self) -> impl Iterator<Item = TerminalId> + '_ {
        self.views
            .values()
            .filter_map(|state| state.transcript.as_ref()?.terminal)
            .map(TerminalId)
    }

    /// Whether a view shows `terminal` as a transcript.
    pub(super) fn shows(&self, terminal: TerminalId) -> bool {
        self.terminals().any(|shown| shown == terminal)
    }

    /// The colour of the dot on a channel's tab: how its newest pipeline
    /// went, in the tone its cards use. None while there is nothing to say.
    fn accent(&self, state: &ChannelState, theme: &Theme) -> Option<[u8; 4]> {
        let channel = self
            .overview
            .as_ref()?
            .channels
            .iter()
            .find(|c| c.repo == state.repo && c.channel == state.channel)?;
        match Tone::of(&channel.status) {
            Tone::Idle => None,
            tone => Some(tone.color(theme).into_rgba8()),
        }
    }

    /// The section's snapshot: the Runners tab, then a locked group per
    /// repository with a tab per channel.
    fn snapshot(&self, theme: &Theme) -> WorkspaceSnapshot {
        let mut items = vec![WorkspaceItem::Tab(TabSnapshot {
            id: TabId(RUNNERS),
            title: "Runners".to_owned(),
            accent_rgba: None,
            root: PaneNode::Leaf {
                pane_id: PaneId(RUNNERS),
                surface: Surface::Ci(RUNNERS),
            },
        })];
        for (repo, known) in &self.repos {
            let tabs = known
                .channels
                .values()
                .filter_map(|view| Some((*view, self.views.get(view)?)))
                .map(|(view, state)| TabSnapshot {
                    id: TabId(view),
                    title: state.channel.clone(),
                    accent_rgba: self.accent(state, theme),
                    root: state.root(view),
                })
                .collect();
            items.push(WorkspaceItem::Group(GroupSnapshot {
                id: GroupId(known.group),
                name: repo.clone(),
                color_rgba: GROUP_GREY,
                locked: true,
                tabs,
            }));
        }
        workspace::ci_snapshot(items)
    }

    /// The view of a channel, with ids minted for it and its repository the
    /// first time either is seen.
    fn view_of(&mut self, repo: &str, channel: &str) -> u64 {
        if !self.repos.contains_key(repo) {
            let group = self.mint();
            self.repos.insert(
                repo.to_owned(),
                Repo {
                    group,
                    channels: BTreeMap::new(),
                },
            );
        }
        if let Some(&view) = self.repos[repo].channels.get(channel) {
            return view;
        }
        let view = self.mint();
        self.views.insert(view, ChannelState::new(repo, channel));
        if let Some(known) = self.repos.get_mut(repo) {
            known.channels.insert(channel.to_owned(), view);
        }
        view
    }

    fn ask_overview(&mut self, key: &RunnerKey) -> Task<Message> {
        if self.overview_pending {
            return Task::none();
        }
        self.overview_pending = true;
        let key = key.clone();
        Task::perform(overview(self.endpoint.clone()), move |result| {
            Message::Ci(key, CiMsg::Overview(result))
        })
    }

    fn overview_arrived(
        &mut self,
        key: &RunnerKey,
        result: Result<CiOverview, String>,
    ) -> Task<Message> {
        self.overview_pending = false;
        let mut tasks = Vec::new();
        match result {
            Ok(overview) => {
                self.error = None;
                // Sorted, so that the same channels get the same ids in every
                // session: a restart brings the tab in front back.
                let mut channels: Vec<(&str, &str)> = overview
                    .channels
                    .iter()
                    .map(|c| (c.repo.as_str(), c.channel.as_str()))
                    .collect();
                channels.sort_unstable();
                for &(repo, channel) in &channels {
                    self.view_of(repo, channel);
                }
                tasks.extend(self.forget_unlisted(&channels));
                self.overview = Some(overview);
                // The first page of a channel no view has loaded yet, and of
                // one whose last attempt failed.
                let behind: Vec<u64> = self
                    .views
                    .iter()
                    .filter(|(_, state)| (!state.loaded || state.stale) && !state.loading)
                    .map(|(view, _)| *view)
                    .collect();
                for view in behind {
                    tasks.push(self.ask_pipelines(key, view, false));
                }
            }
            Err(e) => self.error = Some(e),
        }
        if std::mem::take(&mut self.overview_again) {
            tasks.push(self.ask_overview(key));
        }
        Task::batch(tasks)
    }

    /// Drops the views of channels the runner no longer lists (a repository
    /// taken out of `ci.toml`, a channel whose last pipeline was pruned) and
    /// the groups left empty, closing the transcripts they held. `listed` is
    /// sorted.
    fn forget_unlisted(&mut self, listed: &[(&str, &str)]) -> Vec<Task<Message>> {
        let mut tasks = Vec::new();
        for (repo, known) in &mut self.repos {
            known.channels.retain(|channel, view| {
                if listed
                    .binary_search(&(repo.as_str(), channel.as_str()))
                    .is_ok()
                {
                    return true;
                }
                if let Some(terminal) = self
                    .views
                    .remove(view)
                    .and_then(|state| state.transcript)
                    .and_then(|transcript| transcript.terminal)
                {
                    tasks.push(close(self.endpoint.clone(), terminal));
                }
                false
            });
        }
        self.repos.retain(|_, known| !known.channels.is_empty());
        self.fetching
            .retain(|(repo, _), _| self.repos.contains_key(repo));
        tasks
    }

    /// A channel's first page, or with `older` the page after the last
    /// pipeline shown.
    fn ask_pipelines(&mut self, key: &RunnerKey, view: u64, older: bool) -> Task<Message> {
        let Some(state) = self.views.get_mut(&view) else {
            return Task::none();
        };
        if state.loading {
            return Task::none();
        }
        let before = if older {
            match state.pipelines.last() {
                Some(last) => Some(last.pipeline.number),
                None => return Task::none(),
            }
        } else {
            None
        };
        state.loading = true;
        let (repo, channel) = (state.repo.clone(), state.channel.clone());
        let key = key.clone();
        Task::perform(
            pipelines(self.endpoint.clone(), repo, channel, before),
            move |result| {
                Message::Ci(
                    key,
                    CiMsg::Pipelines {
                        view,
                        older,
                        result,
                    },
                )
            },
        )
    }

    fn pipelines_arrived(
        &mut self,
        view: u64,
        older: bool,
        result: Result<Vec<PipelineView>, String>,
    ) {
        let Some(state) = self.views.get_mut(&view) else {
            return;
        };
        state.loading = false;
        let page = match result {
            Ok(page) => page,
            Err(e) => {
                state.error = Some(e);
                state.stale |= !older;
                return;
            }
        };
        state.error = None;
        state.stale &= older;
        state.loaded = true;
        state.exhausted = page.len() < PAGE as usize;
        // Kept newest first: a runner that sent them otherwise must not break
        // that, nor show one pipeline twice.
        let mut page = page;
        page.sort_by_key(|pipeline| std::cmp::Reverse(pipeline.number));
        page.dedup_by_key(|pipeline| pipeline.number);
        let page = page.into_iter().map(Shown::new);
        if older {
            let last = state
                .pipelines
                .last()
                .map_or(u64::MAX, |shown| shown.pipeline.number);
            state
                .pipelines
                .extend(page.filter(|shown| shown.pipeline.number < last));
        } else {
            state.pipelines = page.collect();
        }
    }

    /// One pipeline again, after the runner said it changed. A second change
    /// while the answer is on its way is asked for once that answer is in.
    fn ask_pipeline(&mut self, key: &RunnerKey, repo: String, number: u64) -> Task<Message> {
        match self.fetching.entry((repo.clone(), number)) {
            Entry::Occupied(mut asked) => {
                *asked.get_mut() = true;
                Task::none()
            }
            Entry::Vacant(slot) => {
                slot.insert(false);
                let key = key.clone();
                Task::perform(
                    pipeline(self.endpoint.clone(), repo.clone(), number),
                    move |result| {
                        Message::Ci(
                            key,
                            CiMsg::Pipeline {
                                repo,
                                number,
                                result,
                            },
                        )
                    },
                )
            }
        }
    }

    fn pipeline_arrived(
        &mut self,
        key: &RunnerKey,
        repo: String,
        number: u64,
        result: Result<Option<Box<PipelineView>>, String>,
    ) -> Task<Message> {
        let again = self
            .fetching
            .remove(&(repo.clone(), number))
            .unwrap_or(false);
        if let Ok(found) = result
            && let Some(known) = self.repos.get(&repo)
        {
            // Every channel of the repository: a pipeline that moved to
            // another channel leaves this one.
            for view in known.channels.values() {
                if let Some(state) = self.views.get_mut(view) {
                    state.changed(number, found.as_deref());
                }
            }
        }
        if again {
            self.ask_pipeline(key, repo, number)
        } else {
            Task::none()
        }
    }

    /// Opens the transcript of a job in a view, in place of the one it shows.
    fn open_job(&mut self, key: &RunnerKey, view: u64, number: u64, job: String) -> Task<Message> {
        let Some(state) = self.views.get_mut(&view) else {
            return Task::none();
        };
        if state
            .transcript
            .as_ref()
            .is_some_and(|t| t.number == number && t.job == job && t.error.is_none())
        {
            return Task::none();
        }
        let mut tasks = Vec::new();
        if let Some(old) = state.transcript.take().and_then(|t| t.terminal) {
            tasks.push(close(self.endpoint.clone(), old));
        }
        state.transcript = Some(Transcript {
            number,
            job: job.clone(),
            terminal: None,
            error: None,
        });
        let repo = state.repo.clone();
        tasks.push(self.ask_transcript(key, view, repo, number, job));
        Task::batch(tasks)
    }

    fn ask_transcript(
        &self,
        key: &RunnerKey,
        view: u64,
        repo: String,
        number: u64,
        job: String,
    ) -> Task<Message> {
        let key = key.clone();
        Task::perform(
            transcript(self.endpoint.clone(), repo, number, job.clone()),
            move |result| {
                Message::Ci(
                    key,
                    CiMsg::Transcript {
                        view,
                        number,
                        job,
                        result,
                    },
                )
            },
        )
    }

    /// What the runner answered a request for a transcript. A job the view
    /// has moved on from gets its terminal closed again.
    fn transcript_arrived(
        &mut self,
        view: u64,
        number: u64,
        job: &str,
        result: Result<u64, String>,
    ) -> Task<Message> {
        let open = self
            .views
            .get_mut(&view)
            .and_then(|state| state.transcript.as_mut())
            .filter(|t| t.number == number && t.job == job && t.terminal.is_none());
        if let Some(transcript) = open {
            match result {
                Ok(terminal) => transcript.terminal = Some(terminal),
                Err(e) => transcript.error = Some(e),
            }
            return Task::none();
        }
        // The view has moved on while the runner answered.
        match result {
            Ok(terminal) if !self.shows(TerminalId(terminal)) => {
                close(self.endpoint.clone(), terminal)
            }
            _ => Task::none(),
        }
    }

    /// Takes a view's transcript away, closing its terminal on the runner.
    fn close_job(&mut self, view: u64) -> Task<Message> {
        let terminal = self
            .views
            .get_mut(&view)
            .and_then(|state| state.transcript.take())
            .and_then(|t| t.terminal);
        match terminal {
            Some(terminal) => close(self.endpoint.clone(), terminal),
            None => Task::none(),
        }
    }

    /// Asks the CI to run a pipeline's commit again, or with `cancel` to stop
    /// it. The pipeline's buttons wait for the answer.
    fn act(&mut self, key: &RunnerKey, view: u64, number: u64, cancel: bool) -> Task<Message> {
        let Some(state) = self.views.get_mut(&view) else {
            return Task::none();
        };
        if !state.acting.insert(number) {
            return Task::none();
        }
        state.notice = None;
        let (key, repo) = (key.clone(), state.repo.clone());
        Task::perform(
            act(self.endpoint.clone(), repo, number, cancel),
            move |result| {
                Message::Ci(
                    key,
                    CiMsg::Acted {
                        view,
                        number,
                        cancel,
                        result,
                    },
                )
            },
        )
    }
}

/// Closes a transcript's terminal and does not wait for the answer: the view
/// has already let go of it, and the runner closes the least recently used
/// one by itself when there are too many.
fn close(endpoint: Endpoint, terminal: u64) -> Task<Message> {
    Task::future(async move {
        if let Err(e) = close_transcript(endpoint, terminal).await {
            eprintln!("[ci] transcript {terminal} not closed: {e}");
        }
    })
    .discard()
}

const UNEXPECTED: &str = "ci: the runner answered another question";

async fn overview(endpoint: Endpoint) -> Result<CiOverview, String> {
    match feed::ci(endpoint, CiRequest::Overview).await? {
        CiReply::Overview { overview } => Ok(overview),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn pipelines(
    endpoint: Endpoint,
    repo: String,
    channel: String,
    before: Option<u64>,
) -> Result<Vec<PipelineView>, String> {
    let request = CiRequest::Pipelines {
        repo,
        channel,
        before,
        limit: PAGE,
    };
    match feed::ci(endpoint, request).await? {
        CiReply::Pipelines { pipelines } => Ok(pipelines),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn pipeline(
    endpoint: Endpoint,
    repo: String,
    number: u64,
) -> Result<Option<Box<PipelineView>>, String> {
    match feed::ci(endpoint, CiRequest::Pipeline { repo, number }).await? {
        CiReply::Pipeline { pipeline } => Ok(pipeline),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn transcript(
    endpoint: Endpoint,
    repo: String,
    number: u64,
    job: String,
) -> Result<u64, String> {
    match feed::ci(endpoint, CiRequest::OpenTranscript { repo, number, job }).await? {
        CiReply::Transcript { terminal } => Ok(terminal),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn close_transcript(endpoint: Endpoint, terminal: u64) -> Result<(), String> {
    match feed::ci(endpoint, CiRequest::CloseTranscript { terminal }).await? {
        CiReply::Closed => Ok(()),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

async fn act(endpoint: Endpoint, repo: String, number: u64, cancel: bool) -> Result<(), String> {
    let request = if cancel {
        CiRequest::Cancel { repo, number }
    } else {
        CiRequest::Rerun { repo, number }
    };
    match feed::ci(endpoint, request).await? {
        CiReply::Accepted => Ok(()),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

impl App {
    /// Brings the CI sections in line with the CI links: one client per
    /// connected CI service, each with its section.
    ///
    /// A client stays while its CI restarts -- its traffic is lost for a
    /// moment, and a CI without a pinned port comes back on another one -- so
    /// the tab in front does not change under the user; it goes when the link
    /// is gone. The overview of a new client is asked for here, which is why
    /// this returns a task.
    pub(super) fn sync_ci_sections(&mut self) -> Task<Message> {
        let ended: Vec<RunnerKey> = self
            .ci
            .keys()
            .filter(|key| {
                self.runtime
                    .links
                    .get(*key)
                    .is_none_or(|link| link.kind != LinkKind::Ci)
            })
            .cloned()
            .collect();
        for key in ended {
            self.ci.remove(&key);
            self.workspace.set_synthetic(RunnerKey::ci(&key), "", None);
        }

        let mut tasks = Vec::new();
        for (key, link) in &self.runtime.links {
            if link.kind == LinkKind::Ci && !self.ci.contains_key(key) {
                let mut client = CiClient::new(link.endpoint.clone(), link.label.clone());
                tasks.push(client.ask_overview(key));
                self.ci.insert(key.clone(), client);
            }
        }

        for (key, client) in &mut self.ci {
            if let Some(link) = self.runtime.links.get(key) {
                client.label.clone_from(&link.label);
                // The same runner (the key is its fingerprint) at a new
                // address; the snapshot that follows resyncs every view.
                if client.endpoint != link.endpoint {
                    client.endpoint = link.endpoint.clone();
                }
            }
            self.workspace.set_synthetic(
                RunnerKey::ci(key),
                &format!("CI {}", client.label),
                Some(client.snapshot(&self.theme)),
            );
        }
        if tasks.is_empty() {
            Task::none()
        } else {
            Task::batch(tasks)
        }
    }

    /// What a runner's CI answered, or what the user did in its views.
    pub(super) fn apply_ci(&mut self, key: RunnerKey, message: CiMsg) -> Task<Message> {
        let Some(client) = self.ci.get_mut(&key) else {
            return Task::none();
        };
        match message {
            CiMsg::Overview(result) => client.overview_arrived(&key, result),
            CiMsg::Pipelines {
                view,
                older,
                result,
            } => {
                client.pipelines_arrived(view, older, result);
                Task::none()
            }
            CiMsg::Pipeline {
                repo,
                number,
                result,
            } => client.pipeline_arrived(&key, repo, number, result),
            CiMsg::LoadOlder { view } => client.ask_pipelines(&key, view, true),
            CiMsg::OpenJob { view, number, job } => {
                let asked = client.open_job(&key, view, number, job);
                // A job opened from the Runners tab is in another tab.
                let tab = TabRef {
                    runner: RunnerKey::ci(&key),
                    tab: TabId(view),
                };
                if self.workspace.active_tab() != Some(&tab) {
                    self.workspace.activate(tab);
                }
                // The transcript this one replaced is no longer wanted.
                Task::batch([asked, self.reconcile_terminals(&key)])
            }
            CiMsg::Transcript {
                view,
                number,
                job,
                result,
            } => {
                let closed = client.transcript_arrived(view, number, &job, result);
                Task::batch([closed, self.reconcile_terminals(&key)])
            }
            CiMsg::CloseTranscript { view } => {
                let closed = client.close_job(view);
                Task::batch([closed, self.reconcile_terminals(&key)])
            }
            CiMsg::Toggle { view, number } => {
                if let Some(state) = client.views.get_mut(&view) {
                    state.toggle(number);
                }
                Task::none()
            }
            CiMsg::Rerun { view, number } => client.act(&key, view, number, false),
            CiMsg::Cancel { view, number } => client.act(&key, view, number, true),
            CiMsg::Acted {
                view,
                number,
                cancel,
                result,
            } => {
                if let Some(state) = client.views.get_mut(&view) {
                    state.acted(number, cancel, result);
                }
                Task::none()
            }
        }
    }

    /// The clock: every CI runner's overview, which is what notices a new
    /// channel and keeps the Runners tab current.
    pub(super) fn poll_ci(&mut self) -> Task<Message> {
        let tasks: Vec<Task<Message>> = self
            .ci
            .iter_mut()
            .map(|(key, client)| client.ask_overview(key))
            .collect();
        Task::batch(tasks)
    }

    /// Whether something on screen counts seconds: the tab in front is a
    /// channel with a running pipeline or job. The one-second clock runs
    /// only while this holds, since every tick redraws the window.
    pub(super) fn ci_clock_wanted(&self) -> bool {
        let Some(front) = self.workspace.active_tab() else {
            return false;
        };
        front
            .runner
            .ci_runner()
            .and_then(|runner| self.ci.get(&runner))
            .and_then(|client| client.views.get(&front.tab.0))
            .is_some_and(ChannelState::ticking)
    }

    /// A pipeline of `repo`'s `channel` changed. The pipeline is asked for
    /// again when a view shows that channel, and the overview always: a
    /// change is how a new pipeline, channel or status shows up.
    pub(super) fn apply_ci_event(
        &mut self,
        key: &RunnerKey,
        repo: String,
        channel: String,
        number: u64,
    ) -> Task<Message> {
        let Some(client) = self.ci.get_mut(key) else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        if client
            .repos
            .get(&repo)
            .is_some_and(|known| known.channels.contains_key(&channel))
        {
            tasks.push(client.ask_pipeline(key, repo, number));
        }
        if client.overview_pending {
            client.overview_again = true;
        } else {
            tasks.push(client.ask_overview(key));
        }
        Task::batch(tasks)
    }

    /// The runner's events resumed after a gap, and nothing says what was
    /// missed: every view loads what it shows again.
    pub(super) fn resync_ci(&mut self, key: &RunnerKey) -> Task<Message> {
        let Some(client) = self.ci.get_mut(key) else {
            return Task::none();
        };
        // Whatever was on its way belongs to the connection that ended.
        client.overview_pending = false;
        client.fetching.clear();
        let mut tasks = vec![client.ask_overview(key)];
        let shown: Vec<u64> = client
            .views
            .iter_mut()
            .filter(|(_, state)| state.loaded || state.loading)
            .map(|(view, state)| {
                state.loading = false;
                *view
            })
            .collect();
        for view in shown {
            tasks.push(client.ask_pipelines(key, view, false));
        }
        tasks.push(self.reopen_ci_transcripts(key));
        Task::batch(tasks)
    }

    /// Asks again for every transcript a view shows. A restarted runner does
    /// not keep transcripts, so their terminals are gone; one that kept
    /// running answers with the same terminal.
    pub(super) fn reopen_ci_transcripts(&mut self, key: &RunnerKey) -> Task<Message> {
        let Some(client) = self.ci.get_mut(key) else {
            return Task::none();
        };
        let mut asks = Vec::new();
        for (view, state) in &mut client.views {
            if let Some(transcript) = &mut state.transcript {
                transcript.terminal = None;
                transcript.error = None;
                asks.push((
                    *view,
                    state.repo.clone(),
                    transcript.number,
                    transcript.job.clone(),
                ));
            }
        }
        let tasks: Vec<Task<Message>> = asks
            .into_iter()
            .map(|(view, repo, number, job)| client.ask_transcript(key, view, repo, number, job))
            .collect();
        Task::batch(tasks)
    }

    /// What a CI pane is called: its section's Runners tab or its channel.
    pub(super) fn ci_pane_title(&self, section: &RunnerKey, view: u64) -> String {
        let client = section.ci_runner().and_then(|runner| self.ci.get(&runner));
        match client {
            Some(_) if view == RUNNERS => "Runners".to_owned(),
            Some(client) => client.views.get(&view).map_or_else(
                || "CI".to_owned(),
                |state| format!("{} / {}", state.repo, state.channel),
            ),
            None => "CI".to_owned(),
        }
    }

    /// What a transcript's terminal pane is called: `<repo> #<n> <job>`.
    pub(super) fn ci_terminal_title(
        &self,
        section: &RunnerKey,
        terminal: TerminalId,
    ) -> Option<String> {
        let client = self.ci.get(&section.ci_runner()?)?;
        client.views.values().find_map(|state| {
            let open = state
                .transcript
                .as_ref()
                .filter(|t| t.terminal == Some(terminal.0))?;
            Some(format!("{} #{} {}", state.repo, open.number, open.job))
        })
    }

    /// One CI pane: the Runners tab, or a channel.
    pub(super) fn ci_view(&self, section: &RunnerKey, view: u64) -> Element<'_, Message, Theme> {
        let Some((runner, client)) = section
            .ci_runner()
            .and_then(|runner| self.ci.get_key_value(&runner))
        else {
            return unavailable("CI", "This section is not a runner's CI.");
        };
        if view == RUNNERS {
            return self.runners_view(runner, client);
        }
        match client.views.get(&view) {
            Some(state) => self.channel_view(runner, view, state),
            None => unavailable("CI", "This view is gone."),
        }
    }

    /// The machines the CI runs jobs on: a table under a toolbar, like a
    /// channel's list.
    fn runners_view<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
    ) -> Element<'a, Message, Theme> {
        let Some(overview) = &client.overview else {
            return unavailable(
                "Runners",
                client.error.as_deref().unwrap_or("Connecting to the CI."),
            );
        };
        let muted = self.theme.muted();
        let runners = overview
            .machines
            .iter()
            .filter(|machine| machine.kind == "runner");
        let online = runners
            .clone()
            .filter(|machine| !matches!(state_tone(&machine.status), Tone::Wait))
            .count();
        let toolbar = row![
            text("Runners").size(TEXT_MD),
            text(format!("{online} of {} online", runners.count()))
                .size(TEXT_SM)
                .color(muted),
        ]
        .spacing(SP_3)
        .align_y(Alignment::Center)
        .padding([SP_2, SP_4])
        .height(TOOLBAR_HEIGHT);
        let head = |label: &'static str| text(label).size(TEXT_SM).color(muted);
        let mut table = column![
            row![
                Space::new().width(ICON_COLUMN),
                head("Machine").width(MACHINE_WIDTH),
                head("State").width(STATE_WIDTH),
                head("Running"),
            ]
            .spacing(SP_3)
        ]
        .spacing(SP_3)
        .padding(SP_4);
        for machine in &overview.machines {
            table = table
                .push(rule::horizontal(1))
                .push(self.machine_row(runner, client, machine));
        }
        let body = scrollable(table).width(Length::Fill).height(Length::Fill);
        column![toolbar, rule::horizontal(1), body].into()
    }

    /// One machine: what kind it is, its name, its state, and what it is
    /// running, each job a link to its transcript.
    fn machine_row<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        machine: &'a MachineView,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let jobs: Element<'a, Message, Theme> = if machine.jobs.is_empty() {
            // Indented like the links, whose padding is their hover area.
            container(text("Nothing").size(TEXT_SM).color(muted))
                .padding([0.0, SP_2])
                .into()
        } else {
            row(machine
                .jobs
                .iter()
                .map(|job| self.machine_job(runner, client, job)))
            .spacing(SP_3)
            .wrap()
            .into()
        };
        let (kind, kind_tip) = if machine.kind == "windows-vm" {
            (glyph::VM, "Windows VM")
        } else {
            (glyph::SERVER, "Runner")
        };
        let tone = state_tone(&machine.status);
        row![
            container(tip(icon(kind, ICON_SIZE, muted), kind_tip.to_owned())).width(ICON_COLUMN),
            text(machine.name.as_str())
                .size(TEXT_MD)
                .width(MACHINE_WIDTH),
            row![
                icon(glyph::DOT, DOT_SIZE, tone.color(&self.theme)),
                text(machine.status.as_str())
                    .size(TEXT_SM)
                    .color(tone.text(&self.theme)),
            ]
            .spacing(SP_2)
            .align_y(Alignment::Center)
            .width(STATE_WIDTH),
            jobs,
        ]
        .spacing(SP_3)
        .align_y(Alignment::Center)
        .into()
    }

    /// `<repo> #<n> <job>`: pressing it brings its channel's tab to the
    /// front with the job's transcript open. A channel this editor does not
    /// show leaves plain text.
    fn machine_job<'a>(
        &'a self,
        runner: &'a RunnerKey,
        client: &'a CiClient,
        job: &'a MachineJob,
    ) -> Element<'a, Message, Theme> {
        let label = text(format!("{} #{} {}", job.repo, job.number, job.job)).size(TEXT_SM);
        let view = client
            .repos
            .get(&job.repo)
            .and_then(|repo| repo.channels.get(&job.channel))
            .copied()
            .filter(|view| client.views.contains_key(view));
        let Some(view) = view else {
            return label.into();
        };
        button(label)
            .padding([SP_1, SP_2])
            .style(link_style)
            .on_press_with(move || {
                Message::Ci(
                    runner.clone(),
                    CiMsg::OpenJob {
                        view,
                        number: job.number,
                        job: job.job.clone(),
                    },
                )
            })
            .into()
    }

    /// A channel: its toolbar over the list of its pipelines.
    fn channel_view<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let mut toolbar = row![
            text(state.repo.as_str()).size(TEXT_MD).color(muted),
            icon(glyph::CHEVRON_RIGHT, TEXT_MD, muted),
            text(state.channel.as_str()).size(TEXT_MD),
            Space::new().width(Length::Fill),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center)
        .padding([SP_2, SP_4])
        .height(TOOLBAR_HEIGHT);
        if let Some(notice) = &state.notice {
            let color = if notice.failed {
                self.theme.error()
            } else {
                muted
            };
            toolbar = toolbar.push(text(notice.text.as_str()).size(TEXT_SM).color(color));
        }
        if !state.pipelines.is_empty()
            && let Some(error) = &state.error
        {
            toolbar = toolbar.push(text(error.as_str()).size(TEXT_SM).color(self.theme.error()));
        }
        if let Some(open) = &state.transcript {
            let what = match &open.error {
                Some(error) => text(format!("Log of #{} {}: {error}", open.number, open.job))
                    .size(TEXT_SM)
                    .color(self.theme.error()),
                None if open.terminal.is_none() => {
                    text(format!("Opening the log of #{} {}", open.number, open.job))
                        .size(TEXT_SM)
                        .color(muted)
                }
                None => text(format!("Log of #{} {}", open.number, open.job))
                    .size(TEXT_SM)
                    .color(muted),
            };
            toolbar = toolbar.push(what).push(tip(
                icon_button(glyph::CLOSE, None).on_press_with(move || {
                    Message::Ci(runner.clone(), CiMsg::CloseTranscript { view })
                }),
                "Close the log".to_owned(),
            ));
        }

        let body: Element<'a, Message, Theme> = if state.pipelines.is_empty() {
            let why = match (&state.error, state.loaded) {
                (Some(error), _) => error.as_str(),
                (None, true) => "No pipelines in this channel yet.",
                (None, false) => "Loading pipelines.",
            };
            unavailable(&state.channel, why)
        } else {
            self.pipeline_list(runner, view, state)
        };
        column![toolbar, rule::horizontal(1), body].into()
    }

    /// The pipelines of a channel, newest on top and separated by rules, and
    /// at the end the button that loads older ones. A pipeline is one row
    /// that says how it went; an open one adds its jobs, why the failed ones
    /// failed, and what can be done about it.
    fn pipeline_list<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
    ) -> Element<'a, Message, Theme> {
        responsive(move |size| {
            let now = now();
            let mut list = column![].width(Length::Fill);
            for (at, shown) in state.pipelines.iter().enumerate() {
                if at > 0 {
                    list = list.push(rule::horizontal(1));
                }
                list = list.push(self.pipeline_entry(runner, view, state, at, shown, size, now));
            }
            if state.loaded && !state.exhausted {
                let older = button(text("Load older pipelines").size(TEXT_SM))
                    .padding([SP_1, SP_3])
                    .style(secondary_style);
                list = list.push(rule::horizontal(1)).push(
                    container(if state.loading {
                        older
                    } else {
                        older.on_press_with(move || {
                            Message::Ci(runner.clone(), CiMsg::LoadOlder { view })
                        })
                    })
                    .padding(SP_4),
                );
            }
            scrollable(list)
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().spacing(SP_1),
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        })
        .into()
    }

    /// One pipeline: its row, and below it, when open, its jobs.
    #[allow(clippy::too_many_arguments)]
    fn pipeline_entry<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
        at: usize,
        shown: &'a Shown,
        size: Size,
        now: u64,
    ) -> Element<'a, Message, Theme> {
        let pipeline = &shown.pipeline;
        let open = state.is_open(at, pipeline);
        let number = pipeline.number;
        let row = button(self.pipeline_row(pipeline, open, size, now))
            .width(Length::Fill)
            .padding([SP_3, SP_4])
            .style(row_style)
            .on_press_with(move || Message::Ci(runner.clone(), CiMsg::Toggle { view, number }));
        if !open {
            return row.into();
        }
        column![
            row,
            container(self.pipeline_detail(runner, view, state, shown, now)).padding(Padding {
                top: 0.0,
                right: SP_4,
                bottom: SP_4,
                left: SP_4 + ICON_COLUMN + SP_3,
            }),
        ]
        .into()
    }

    /// What a pipeline's row says: how it went, what it built and who
    /// started it, why it failed, and when.
    fn pipeline_row<'a>(
        &'a self,
        pipeline: &'a PipelineView,
        open: bool,
        size: Size,
        now: u64,
    ) -> Element<'a, Message, Theme> {
        let theme = &self.theme;
        let muted = theme.muted();
        let tone = Tone::of(&pipeline.status);
        // Room for the subject: the pane less the icon, the times, the
        // padding and a closed pipeline's glyph per job, at an average glyph
        // of half the text size.
        let pips = if open {
            0.0
        } else {
            pipeline.jobs.len() as f32 * (TEXT_MD + SP_1) + 2.0 * SP_2
        };
        let room = (size.width - ICON_COLUMN - TIME_COLUMN - 4.0 * SP_4 - pips).max(120.0);
        let chars = (room / (TEXT_MD * 0.5)) as usize;
        let subject = if pipeline.subject.is_empty() {
            pipeline.note.as_str()
        } else {
            pipeline.subject.as_str()
        };
        let mut title = row![
            text(format!("#{}", pipeline.number))
                .size(TEXT_MD)
                .color(muted)
                .wrapping(Wrapping::None),
            text(fit(subject, chars.saturating_sub(6)))
                .size(TEXT_MD)
                .wrapping(Wrapping::None),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center);
        if !open {
            title = title
                .push(Space::new().width(SP_2))
                .push(job_pips(theme, pipeline));
        }
        let mut lines = column![title, self.trigger_line(pipeline, chars)].spacing(SP_1);
        for job in pipeline
            .jobs
            .iter()
            .filter(|job| Tone::of(&job.status) == Tone::Bad)
        {
            lines = lines.push(self.cause_line(job, chars));
        }

        let elapsed = match pipeline.finished {
            Some(finished) if finished >= pipeline.created => duration(finished - pipeline.created),
            None if tone == Tone::Active => duration(now.saturating_sub(pipeline.created)),
            _ => String::new(),
        };
        let times = column![
            text(elapsed)
                .size(TEXT_SM)
                .color(tone.text(theme))
                .wrapping(Wrapping::None),
            tip(
                text(ago(pipeline.created, now))
                    .size(TEXT_SM)
                    .color(muted)
                    .wrapping(Wrapping::None),
                local(pipeline.created),
            ),
        ]
        .spacing(SP_1)
        .align_x(Alignment::End)
        .width(TIME_COLUMN);
        row![
            container(tip(
                icon(status_glyph(&pipeline.status), ICON_SIZE, tone.color(theme)),
                pipeline.status.clone(),
            ))
            .width(ICON_COLUMN)
            .padding(Padding {
                top: 1.0,
                ..Padding::ZERO
            }),
            container(lines).width(Length::Fill).clip(true),
            times,
        ]
        .spacing(SP_3)
        .align_y(Alignment::Start)
        .into()
    }

    /// `<branch> <commit> <who>`, each behind its glyph.
    fn trigger_line<'a>(
        &'a self,
        pipeline: &'a PipelineView,
        chars: usize,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let part = |glyph: char, words: String| {
            row![
                icon(glyph, TEXT_SM, muted),
                text(words)
                    .size(TEXT_SM)
                    .color(muted)
                    .wrapping(Wrapping::None)
            ]
            .spacing(SP_1)
            .align_y(Alignment::Center)
        };
        let (glyph, what) = match pipeline.event.as_str() {
            "tag" => (glyph::TAG, pipeline.git_ref.clone()),
            "cron" => (glyph::CRON, format!("nightly on {}", pipeline.git_ref)),
            _ => (glyph::BRANCH, pipeline.git_ref.clone()),
        };
        let mut line = row![part(glyph, fit(&what, chars / 2))]
            .spacing(SP_3)
            .align_y(Alignment::Center);
        if let Some(sha) = pipeline.sha.as_deref() {
            line = line.push(part(glyph::COMMIT, sha.get(..7).unwrap_or(sha).to_owned()));
        }
        // A cron's actor is `cron`, which the trigger already says.
        if !pipeline.actor.is_empty() && pipeline.actor != pipeline.event {
            line = line.push(part(glyph::PERSON, pipeline.actor.clone()));
        }
        line.into()
    }

    /// `<job>  <cause>` of a failed job, in the colour of a failure.
    fn cause_line<'a>(&'a self, job: &'a JobView, chars: usize) -> Element<'a, Message, Theme> {
        let tone = Tone::Bad.text(&self.theme);
        let cause = job.cause.clone().unwrap_or_else(|| failure_note(job));
        row![
            icon(glyph::ERROR, TEXT_SM, Tone::Bad.color(&self.theme)),
            text(job.name.as_str())
                .size(TEXT_SM)
                .color(tone)
                .wrapping(Wrapping::None),
            text(fit(&cause, chars.saturating_sub(job.name.len() + 4)))
                .size(TEXT_SM)
                .font(iced_terminal::FONT)
                .color(tone)
                .wrapping(Wrapping::None),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center)
        .into()
    }

    /// An open pipeline: what can be done about it, its jobs, and for each
    /// failed job the end of its log.
    fn pipeline_detail<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
        shown: &'a Shown,
        now: u64,
    ) -> Element<'a, Message, Theme> {
        let pipeline = &shown.pipeline;
        let number = pipeline.number;
        let acting = state.acting.contains(&number);
        let rerun = action_button(glyph::RERUN, "Rerun");
        let mut actions = row![tip(
            if acting {
                rerun
            } else {
                rerun.on_press_with(move || {
                    Message::Ci(runner.clone(), CiMsg::Rerun { view, number })
                })
            },
            "Run this commit again as a new pipeline".to_owned(),
        )]
        .spacing(SP_2)
        .align_y(Alignment::Center);
        if pipeline.finished.is_none() && pipeline.status == "running" {
            let cancel = action_button(glyph::STOP, "Cancel");
            actions = actions.push(tip(
                if acting {
                    cancel
                } else {
                    cancel.on_press_with(move || {
                        Message::Ci(runner.clone(), CiMsg::Cancel { view, number })
                    })
                },
                "Stop the running jobs and skip the rest".to_owned(),
            ));
        }
        if !pipeline.note.is_empty() && !pipeline.subject.is_empty() {
            let color = match Tone::of(&pipeline.status) {
                Tone::Bad => self.theme.error(),
                _ => self.theme.muted(),
            };
            actions = actions.push(
                text(flatten(&pipeline.note, usize::MAX))
                    .size(TEXT_SM)
                    .color(color),
            );
        }
        let mut detail = column![actions].spacing(SP_3);
        if !shown.jobs.cards.is_empty() {
            detail = detail.push(
                scrollable(self.job_graph(runner, view, state, shown, now))
                    .direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::default(),
                    ))
                    .width(Length::Fill),
            );
        }
        for job in pipeline
            .jobs
            .iter()
            .filter(|job| Tone::of(&job.status) == Tone::Bad)
        {
            detail = detail.push(self.failure(runner, view, number, job));
        }
        detail.into()
    }

    /// Why a job failed: its cause, the end of its log as the CI kept it, and
    /// the button that opens the whole log.
    fn failure<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        number: u64,
        job: &'a JobView,
    ) -> Element<'a, Message, Theme> {
        let theme = &self.theme;
        let head = row![
            icon(glyph::ERROR, ICON_SIZE, Tone::Bad.color(theme)),
            text(format!("{} failed", job.name)).size(TEXT_MD),
            text(failure_note(job)).size(TEXT_SM).color(theme.muted()),
            Space::new().width(Length::Fill),
            action_button(glyph::LOG, "Open log").on_press_with(move || {
                Message::Ci(
                    runner.clone(),
                    CiMsg::OpenJob {
                        view,
                        number,
                        job: job.name.clone(),
                    },
                )
            }),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center);
        let mut block = column![head].spacing(SP_2);
        if let Some(excerpt) = job.excerpt.as_deref() {
            let lines = excerpt_lines(excerpt, job.cause.as_deref(), EXCERPT_LINES);
            if !lines.is_empty() {
                block = block.push(
                    container(
                        text(lines)
                            .size(TEXT_SM)
                            .font(iced_terminal::FONT)
                            .wrapping(Wrapping::None),
                    )
                    .width(Length::Fill)
                    .clip(true)
                    .padding([SP_2, SP_3])
                    .style(|theme: &Theme| {
                        let palette = theme.extended();
                        container::Style {
                            background: Some(palette.background.weakest.color.into()),
                            text_color: Some(palette.background.base.text),
                            border: iced::Border {
                                color: palette.background.weak.color,
                                width: 1.0,
                                radius: RADIUS.into(),
                            },
                            ..container::Style::default()
                        }
                    }),
                );
            }
        }
        block.into()
    }

    /// A pipeline's jobs: the cables at the bottom, a card per job over
    /// them. Only the cards take input.
    fn job_graph<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
        shown: &'a Shown,
        now: u64,
    ) -> Element<'a, Message, Theme> {
        let graph = &shown.jobs;
        let number = shown.pipeline.number;
        let mut layers: Vec<Element<'a, Message, Theme>> =
            Vec::with_capacity(graph.cards.len() + 1);
        layers.push(
            Cables::new(
                graph.size,
                &graph.cables,
                &graph.ports,
                self.theme.extended().background.strong.color,
            )
            .into(),
        );
        for (job, &at) in shown.pipeline.jobs.iter().zip(&graph.cards) {
            let selected = state
                .transcript
                .as_ref()
                .is_some_and(|open| open.number == number && open.job == job.name);
            layers.push(
                pin(self.job_card(runner, view, number, job, selected, now))
                    .position(at)
                    .into(),
            );
        }
        stack(layers)
            .width(graph.size.width)
            .height(graph.size.height)
            .into()
    }

    /// A job's card: how its run went and its name, and under them how long
    /// it took and where, or what it waits for. Pressing it opens the job's
    /// log; the card whose log is open is marked.
    fn job_card<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        number: u64,
        job: &'a JobView,
        selected: bool,
        now: u64,
    ) -> Element<'a, Message, Theme> {
        let theme = &self.theme;
        let tone = Tone::of(&job.status);
        let detail = job_detail(job, now);
        let card = row![
            icon(status_glyph(&job.status), ICON_SIZE, tone.color(theme)),
            column![
                text(job.name.as_str())
                    .size(TEXT_MD)
                    .wrapping(Wrapping::None),
                text(fit(&detail, CARD_CHARS))
                    .size(TEXT_SM)
                    .color(match tone {
                        Tone::Bad | Tone::Wait => tone.text(theme),
                        _ => theme.muted(),
                    })
                    .wrapping(Wrapping::None),
            ]
            .spacing(SP_1),
        ]
        .spacing(SP_2)
        .align_y(Alignment::Center);
        tip(
            button(card)
                .width(JOB_WIDTH)
                .height(JOB_HEIGHT)
                .padding([SP_2, SP_3])
                .clip(true)
                .style(move |theme: &Theme, status| tone.card(theme, status, selected))
                .on_press_with(move || {
                    Message::Ci(
                        runner.clone(),
                        CiMsg::OpenJob {
                            view,
                            number,
                            job: job.name.clone(),
                        },
                    )
                }),
            job_tip(job),
        )
    }
}

/// Codicons of the bundled Nerd Font, the chrome's one icon set.
mod glyph {
    pub const PASS: char = '\u{ebb3}';
    pub const ERROR: char = '\u{ea87}';
    pub const RUNNING: char = '\u{ea77}';
    pub const PENDING: char = '\u{ebb5}';
    pub const WAITING: char = '\u{eb7c}';
    pub const FROZEN: char = '\u{ead1}';
    pub const SKIPPED: char = '\u{eabd}';
    pub const CANCELLED: char = '\u{eba5}';
    pub const BRANCH: char = '\u{ea68}';
    pub const TAG: char = '\u{ea66}';
    pub const CRON: char = '\u{eab0}';
    pub const COMMIT: char = '\u{eafc}';
    pub const PERSON: char = '\u{ea67}';
    pub const RERUN: char = '\u{ebc0}';
    pub const STOP: char = '\u{ead7}';
    pub const LOG: char = '\u{eb9d}';
    pub const CLOSE: char = '\u{ea76}';
    pub const SERVER: char = '\u{eb50}';
    pub const VM: char = '\u{ea7a}';
    pub const CHEVRON_RIGHT: char = '\u{eab6}';
    pub const DOT: char = '\u{ea71}';
}

/// The bundled face is a Mono Nerd Font, which squeezes every icon into one
/// cell, about half an em wide: drawn at this many times the size, a glyph
/// is as wide as the size asks for.
const GLYPH_SCALE: f32 = 1.6;

/// A glyph of the icon font, `size` pixels wide, on a line as tall.
fn glyph_text<'a>(glyph: char, size: f32) -> iced::widget::Text<'a, Theme> {
    text(glyph.to_string())
        .font(iced_terminal::FONT)
        .size(size * GLYPH_SCALE)
        .line_height(iced::widget::text::LineHeight::Absolute(size.into()))
}

/// A glyph of the icon font in `color`.
fn icon<'a>(glyph: char, size: f32, color: Color) -> Element<'a, Message, Theme> {
    glyph_text(glyph, size).color(color).into()
}

/// What a status looks like: the glyph that tells it apart without colour.
fn status_glyph(status: &str) -> char {
    match status {
        "succeeded" => glyph::PASS,
        "failed" | "error" => glyph::ERROR,
        "running" | "starting" => glyph::RUNNING,
        "waiting" => glyph::WAITING,
        "frozen" => glyph::FROZEN,
        "skipped" | "no-jobs" => glyph::SKIPPED,
        "cancelled" => glyph::CANCELLED,
        _ => glyph::PENDING,
    }
}

/// One small status glyph per job: a collapsed pipeline's jobs at a glance.
fn job_pips<'a>(theme: &Theme, pipeline: &'a PipelineView) -> Element<'a, Message, Theme> {
    row(pipeline.jobs.iter().map(|job| {
        let tone = Tone::of(&job.status);
        tip(
            icon(status_glyph(&job.status), TEXT_MD, tone.color(theme)),
            format!("{}: {}", job.name, job.status),
        )
    }))
    .spacing(SP_1)
    .into()
}

/// The second line of a job's card.
fn job_detail(job: &JobView, now: u64) -> String {
    let tone = Tone::of(&job.status);
    let took = match (job.started, job.finished) {
        (Some(started), Some(finished)) if finished >= started => {
            Some(duration(finished - started))
        }
        (Some(started), None) if tone == Tone::Active => {
            Some(duration(now.saturating_sub(started)))
        }
        _ => None,
    };
    // The runner a job runs on, without the image or machine after it.
    let place = job.place.split(',').next().unwrap_or_default().trim();
    match tone {
        Tone::Bad => failure_note(job),
        Tone::Good | Tone::Active => match took {
            Some(took) if !place.is_empty() => format!("{took} on {place}"),
            Some(took) => took,
            None => place.to_owned(),
        },
        Tone::Wait | Tone::Idle if !job.note.is_empty() => flatten(&job.note, NOTE_CHARS),
        Tone::Wait | Tone::Idle => job.status.clone(),
    }
}

/// A job card's tooltip: everything the card has no room for.
fn job_tip(job: &JobView) -> String {
    let mut words = format!("{}: {}", job.name, job.status);
    if !job.place.is_empty() {
        words.push_str(&format!("\non {}", job.place));
    }
    if !job.note.is_empty() {
        words.push('\n');
        words.push_str(&flatten(&job.note, usize::MAX));
    }
    words.push_str("\nPress to open its log");
    words
}

/// `exit 101 after 5s`, `timed out`, or the CI's note: how a job failed,
/// short.
fn failure_note(job: &JobView) -> String {
    let took = match (job.started, job.finished) {
        (Some(started), Some(finished)) if finished >= started => {
            format!(" after {}", duration(finished - started))
        }
        _ => String::new(),
    };
    match job.code {
        Some(code) => format!("exit {code}{took}"),
        None if !job.note.is_empty() => flatten(&job.note, NOTE_CHARS),
        None => format!("{}{took}", job.status),
    }
}

/// The part of an excerpt worth showing: all of it when it fits in `limit`
/// lines, else from just before its cause line on, or its last lines when
/// it names no cause.
fn excerpt_lines(excerpt: &str, cause: Option<&str>, limit: usize) -> String {
    let lines: Vec<&str> = excerpt
        .lines()
        .map(str::trim_end)
        .skip_while(|line| line.is_empty())
        .collect();
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |at| at + 1);
    let lines = &lines[..end];
    // The cause the CI reports may run on past its line: the line that
    // starts it is the one it starts with.
    let cause_at = cause.map(str::trim).and_then(|cause| {
        lines.iter().position(|line| {
            let line = line.trim();
            !line.is_empty() && (line.contains(cause) || cause.starts_with(line))
        })
    });
    let start = match cause_at {
        _ if lines.len() <= limit => 0,
        Some(at) => at.saturating_sub(EXCERPT_CONTEXT),
        None => lines.len() - limit,
    };
    lines[start..lines.len().min(start + limit)].join("\n")
}

/// `text` on one line, shortened to `limit` characters with an ellipsis.
fn fit(text: &str, limit: usize) -> String {
    let line = flatten(text, usize::MAX);
    if line.chars().count() <= limit.max(1) {
        return line;
    }
    let mut cut: String = line.chars().take(limit.saturating_sub(1).max(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// A text button with a glyph in front of its words.
fn action_button<'a>(glyph: char, words: &'a str) -> button::Button<'a, Message, Theme> {
    icon_button(glyph, Some(words))
}

/// A button of the toolbar or a pipeline's actions: a glyph, and words
/// after it when there are any.
fn icon_button<'a>(glyph: char, words: Option<&'a str>) -> button::Button<'a, Message, Theme> {
    // No colour of its own: the glyph takes the button's text colour, which
    // follows the button's state.
    let mut content = row![glyph_text(glyph, TEXT_MD)]
        .spacing(SP_2)
        .align_y(Alignment::Center);
    if let Some(words) = words {
        content = content.push(text(words).size(TEXT_SM));
    }
    button(content)
        .padding([SP_1 + 1.0, SP_2])
        .height(BUTTON_HEIGHT)
        .style(secondary_style)
}

/// A quiet button: the background of the surface, a border that says it
/// can be pressed, and a step lighter under the pointer.
fn secondary_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended();
    let (background, border) = match status {
        button::Status::Hovered => (
            palette.background.weak.color,
            palette.background.strong.color,
        ),
        button::Status::Pressed => (
            palette.background.strong.color,
            palette.background.strong.color,
        ),
        button::Status::Active => (
            palette.background.weaker.color,
            palette.background.weak.color,
        ),
        button::Status::Disabled => (palette.background.base.color, palette.background.weak.color),
    };
    button::Style {
        background: Some(background.into()),
        text_color: match status {
            button::Status::Disabled => theme.muted(),
            _ => palette.background.base.text,
        },
        border: iced::Border {
            color: border,
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..button::Style::default()
    }
}

/// A pipeline's row: flat, a shade lighter under the pointer.
fn row_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended();
    let background = match status {
        button::Status::Hovered => Some(palette.background.weakest.color.into()),
        button::Status::Pressed => Some(palette.background.weaker.color.into()),
        button::Status::Active | button::Status::Disabled => None,
    };
    button::Style {
        background,
        text_color: palette.background.base.text,
        ..button::Style::default()
    }
}

/// A job named in the Runners table: link coloured, underlined by a
/// background under the pointer.
fn link_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended();
    let (background, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (
            Some(palette.background.weak.color.into()),
            palette.background.base.text,
        ),
        _ => (None, Tone::Active.text(theme)),
    };
    button::Style {
        background,
        text_color,
        border: iced::Border {
            radius: RADIUS.into(),
            ..iced::Border::default()
        },
        ..button::Style::default()
    }
}

/// How a run went: the colour of a job card's outline and of a status word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Good,
    Bad,
    Active,
    /// Held back: waiting for a runner, paused while it is busy.
    Wait,
    Idle,
}

impl Tone {
    fn of(status: &str) -> Tone {
        match status {
            "succeeded" => Tone::Good,
            "failed" | "error" => Tone::Bad,
            "running" | "starting" => Tone::Active,
            "waiting" | "frozen" => Tone::Wait,
            _ => Tone::Idle,
        }
    }

    fn color(self, theme: &Theme) -> Color {
        let palette = theme.extended();
        match self {
            Tone::Good => palette.success.base.color,
            Tone::Bad => palette.danger.base.color,
            Tone::Active => palette.primary.base.color,
            Tone::Wait => palette.warning.base.color,
            Tone::Idle => theme.muted(),
        }
    }

    /// A status word: the hue mixed towards the foreground, since a border
    /// colour is too dark to read as text on a dark theme; an idle one is
    /// plain secondary text.
    fn text(self, theme: &Theme) -> Color {
        match self {
            Tone::Idle => theme.muted(),
            _ => iced::theme::palette::mix(
                self.color(theme),
                theme.extended().background.base.text,
                0.25,
            ),
        }
    }

    /// A job's card. Opaque, because the cables run under the cards, and a
    /// step lighter under the pointer so it reads as pressable. The card
    /// whose log is open is filled with a tint of the accent and keeps the
    /// outline of its status, so a running card stays told apart from it.
    fn card(self, theme: &Theme, status: button::Status, selected: bool) -> button::Style {
        let palette = theme.extended();
        let base = match status {
            button::Status::Hovered => palette.background.weak.color,
            button::Status::Pressed => palette.background.neutral.color,
            button::Status::Active | button::Status::Disabled => palette.background.weaker.color,
        };
        let background = if selected {
            iced::theme::palette::mix(base, palette.primary.base.color, 0.22)
        } else {
            base
        };
        let color = match (selected, self) {
            (true, Tone::Idle) => palette.primary.base.color,
            (false, Tone::Idle) => palette.background.strong.color,
            (_, tone) => tone.color(theme),
        };
        button::Style {
            background: Some(background.into()),
            text_color: palette.background.base.text,
            border: iced::Border {
                color,
                width: 1.0,
                radius: RADIUS.into(),
            },
            ..Default::default()
        }
    }
}

/// `45s`, `3m 12s`, `1h 05m`.
fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// The colour family of a machine's state word: ready, working, or not
/// there for jobs.
fn state_tone(state: &str) -> Tone {
    // `free (set)`: the word that says how it is, not how it got there.
    match state.split_whitespace().next() {
        Some("free" | "ready" | "online") => Tone::Good,
        Some("booting" | "stopping") => Tone::Active,
        Some("busy" | "offline") => Tone::Wait,
        _ => Tone::Idle,
    }
}

/// Seconds since the UNIX epoch, the clock `PipelineView.created` is read
/// against.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
fn ago(then: u64, now: u64) -> String {
    match now.saturating_sub(then) {
        0..60 => "just now".to_owned(),
        seconds @ 60..3600 => format!("{}m ago", seconds / 60),
        seconds @ 3600..86_400 => format!("{}h ago", seconds / 3600),
        seconds => format!("{}d ago", seconds / 86_400),
    }
}

/// The local date and time of an epoch second, to the second.
fn local(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map_or_else(
            || format!("{seconds} s after the epoch"),
            |time| {
                time.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            },
        )
}

/// `content` with `words` in a bubble below it, styled as the tab bar's
/// tooltips are.
fn tip<'a>(
    content: impl Into<Element<'a, Message, Theme>>,
    words: String,
) -> Element<'a, Message, Theme> {
    let bubble = container(text(words).size(TEXT_SM))
        .padding([3, 6])
        .style(|theme: &Theme| {
            let style = iced_tabs::Catalog::tooltip(theme);
            container::Style {
                background: style.background,
                text_color: Some(style.text),
                border: style.border,
                ..container::Style::default()
            }
        });
    tooltip(content, bubble, tooltip::Position::Bottom)
        .gap(SP_1)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_steps_at_each_unit_boundary() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 159), "just now");
        assert_eq!(ago(100, 160), "1m ago");
        assert_eq!(ago(0, 3599), "59m ago");
        assert_eq!(ago(0, 3600), "1h ago");
        assert_eq!(ago(0, 86_399), "23h ago");
        assert_eq!(ago(0, 86_400), "1d ago");
        // A runner's clock ahead of this one is not a negative age.
        assert_eq!(ago(500, 100), "just now");
    }

    #[test]
    fn state_words_keep_their_tone_with_a_set_marker() {
        assert!(matches!(state_tone("free (set)"), Tone::Good));
        assert!(matches!(state_tone("busy (set)"), Tone::Wait));
        assert!(matches!(state_tone("offline"), Tone::Wait));
        assert!(matches!(state_tone("off"), Tone::Idle));
    }

    #[test]
    fn an_excerpt_shows_its_cause_with_context_or_its_tail() {
        let excerpt = "a\nb\nc\nd\nerror[E0308]: mismatched types\n  --> x.rs:1\n\nerror: could not compile\n\n";
        // Two lines before the cause, then as many as fit.
        assert_eq!(
            excerpt_lines(excerpt, Some("error[E0308]: mismatched types"), 3),
            "c\nd\nerror[E0308]: mismatched types"
        );
        // A cause that runs on past its line is found by the line it starts
        // with.
        assert_eq!(
            excerpt_lines(excerpt, Some("error: could not compile `x` (lib)"), 3),
            "  --> x.rs:1\n\nerror: could not compile"
        );
        // Without a cause, the last lines, trailing blank ones dropped.
        assert_eq!(
            excerpt_lines(excerpt, None, 2),
            "\nerror: could not compile"
        );
        // A cause the excerpt does not hold falls back to the tail too.
        assert_eq!(
            excerpt_lines(excerpt, Some("panicked at"), 1),
            "error: could not compile"
        );
        // What fits is shown whole.
        assert_eq!(excerpt_lines("x\ny\n", Some("y"), 5), "x\ny");
    }

    #[test]
    fn fit_cuts_long_lines_with_an_ellipsis_and_keeps_short_ones() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("exactly10!", 10), "exactly10!");
        assert_eq!(fit("a line that is too long", 10), "a line th\u{2026}");
        assert_eq!(fit("two\nlines", 20), "two lines");
    }

    /// The newest pipeline and running ones are open until the user closes
    /// them; any other opens on demand.
    #[test]
    fn a_pipeline_is_open_by_default_when_newest_or_running_and_toggles() {
        let pipeline = |number: u64, status: &str, finished: Option<u64>| PipelineView {
            repo: "r".to_owned(),
            number,
            channel: "dev".to_owned(),
            event: "push".to_owned(),
            git_ref: "main".to_owned(),
            sha: None,
            actor: String::new(),
            subject: String::new(),
            status: status.to_owned(),
            note: String::new(),
            created: 0,
            finished,
            jobs: Vec::new(),
        };
        let mut state = ChannelState::new("r", "dev");
        let newest = pipeline(9, "failed", Some(5));
        let running = pipeline(8, "running", None);
        let old = pipeline(7, "succeeded", Some(3));
        assert!(state.is_open(0, &newest));
        assert!(state.is_open(1, &running));
        assert!(!state.is_open(2, &old));
        state.toggle(9);
        state.toggle(7);
        assert!(!state.is_open(0, &newest));
        assert!(state.is_open(2, &old));
        state.toggle(7);
        assert!(!state.is_open(2, &old));
    }

    /// A repository taken out of the runner's configuration leaves the tab
    /// bar with the next overview, and the channels that stay keep their
    /// views, so their tabs stay where they are.
    #[test]
    fn channels_the_overview_drops_lose_their_views_and_groups() {
        let listing = |channels: &[(&str, &str)]| CiOverview {
            channels: channels
                .iter()
                .map(|&(repo, channel)| zeughaus_link::ChannelView {
                    repo: repo.to_owned(),
                    channel: channel.to_owned(),
                    latest: 1,
                    status: "succeeded".to_owned(),
                })
                .collect(),
            machines: Vec::new(),
        };
        let key = RunnerKey::new("sha256:ci");
        let mut client = CiClient::new(Endpoint("weida://ci/".to_owned()), "ci".to_owned());
        let _ = client.overview_arrived(
            &key,
            Ok(listing(&[
                ("griasdi", "main"),
                ("smoke", "cron"),
                ("smoke", "main"),
            ])),
        );
        let main = client.repos["griasdi"].channels["main"];
        assert_eq!(client.views.len(), 3);

        let _ = client.overview_arrived(&key, Ok(listing(&[("griasdi", "main")])));
        assert!(!client.repos.contains_key("smoke"));
        assert_eq!(client.views.keys().copied().collect::<Vec<_>>(), vec![main]);
        assert_eq!(client.repos["griasdi"].channels["main"], main);
    }
}
