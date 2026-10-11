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
use std::collections::{BTreeMap, HashMap};

use iced::widget::text::Wrapping;
use iced::widget::{button, column, pin, responsive, row, rule, scrollable, stack, text};
use iced::{Alignment, Color, Element, Length, Point, Size, Task};
use iced_nodegraph_sdf::Shape;
use zeughaus_core::NodeId;
use zeughaus_link::{CiOverview, CiReply, CiRequest, JobView, PipelineView};
use zeughaus_mux::{
    Axis, GroupId, GroupSnapshot, PaneId, PaneNode, SplitId, TabId, TabSnapshot, TerminalId,
    WorkspaceItem, WorkspaceSnapshot,
};
use zeughaus_theme::Theme;

use self::cables::{Cables, cable};
use super::layout::auto_cells;
use super::{App, unavailable};
use crate::feed;
use crate::message::{CiMsg, Message};
use crate::transport::Endpoint;
use crate::workspace::{self, RunnerKey, Surface};

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
const JOB_WIDTH: f32 = 200.0;
const JOB_HEIGHT: f32 = 96.0;
/// Space between two columns of cards: the room a cable bends in.
const COLUMN_GAP: f32 = 72.0;
/// Space between two cards of one column.
const ROW_GAP: f32 = 16.0;
/// Characters of a job's note its card shows.
const NOTE_CHARS: usize = 60;

/// The spacing scale of a channel's list.
const SP_1: f32 = 4.0;
const SP_2: f32 = 8.0;
const SP_3: f32 = 12.0;
const SP_4: f32 = 16.0;
/// Text sizes: secondary lines, and everything else.
const TEXT_SM: f32 = 12.0;
const TEXT_MD: f32 = 14.0;

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
/// them.
struct ChannelState {
    repo: String,
    channel: String,
    /// `<channel>@<repo>`: the tab's title and the view's heading.
    title: String,
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

/// `#<n>  push to <ref>  <sha7>  by <actor>`: what started the pipeline.
fn summary(pipeline: &PipelineView) -> String {
    let trigger = match pipeline.event.as_str() {
        "push" => format!("push to {}", pipeline.git_ref),
        "cron" => format!("cron on {}", pipeline.git_ref),
        event => format!("{event} {}", pipeline.git_ref),
    };
    let sha = pipeline
        .sha
        .as_deref()
        .map(|sha| sha.get(..7).unwrap_or(sha))
        .unwrap_or_default();
    // A cron's actor is `cron`, which the trigger already says.
    let actor = if pipeline.actor.is_empty() || pipeline.actor == pipeline.event {
        String::new()
    } else {
        format!("by {}", pipeline.actor)
    };
    let mut parts = vec![
        format!("#{}", pipeline.number),
        trigger,
        sha.to_owned(),
        actor,
    ];
    parts.retain(|part| !part.trim().is_empty());
    parts.join("  ")
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
            title: format!("{channel}@{repo}"),
            pipelines: Vec::new(),
            loaded: false,
            loading: false,
            stale: false,
            exhausted: false,
            error: None,
            transcript: None,
        }
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

    /// The section's snapshot: the Runners tab, then a locked group per
    /// repository with a tab per channel.
    fn snapshot(&self) -> WorkspaceSnapshot {
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
                    title: state.title.clone(),
                    accent_rgba: None,
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
                for (repo, channel) in channels {
                    self.view_of(repo, channel);
                }
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

impl App {
    /// Brings the CI sections in line with the runners: one client per
    /// connected runner that runs CI, each with its section.
    ///
    /// A runner that runs CI is one that reported a machine. A client stays
    /// while its runner restarts -- its traffic is lost and the machine with
    /// it for a moment, and a runner without a pinned port comes back on
    /// another one -- so the tab in front does not change under the user;
    /// it goes when the runner is gone or reports in again without a
    /// machine. The overview of a new client is asked for here, which is
    /// why this returns a task.
    pub(super) fn sync_ci_sections(&mut self) -> Task<Message> {
        let ended: Vec<RunnerKey> = self
            .ci
            .iter()
            .filter(|(key, _)| {
                self.runtime
                    .links
                    .get(*key)
                    .is_none_or(|link| link.traffic_live && link.machine.is_none())
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in ended {
            self.ci.remove(&key);
            self.workspace.set_synthetic(RunnerKey::ci(&key), "", None);
        }

        let mut tasks = Vec::new();
        for (key, link) in &self.runtime.links {
            if link.machine.is_some() && !self.ci.contains_key(key) {
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
                Some(client.snapshot()),
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
            Some(client) => client
                .views
                .get(&view)
                .map_or_else(|| "CI".to_owned(), |state| state.title.clone()),
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
            return self.runners_view(client);
        }
        match client.views.get(&view) {
            Some(state) => self.channel_view(runner, view, state),
            None => unavailable("CI", "This view is gone."),
        }
    }

    /// The machines the CI runs jobs on, one row each.
    fn runners_view<'a>(&'a self, client: &'a CiClient) -> Element<'a, Message, Theme> {
        let Some(overview) = &client.overview else {
            return unavailable(
                "Runners",
                client.error.as_deref().unwrap_or("Waiting for the CI."),
            );
        };
        let muted = self.theme.muted();
        let rows = overview.machines.iter().map(|machine| {
            let jobs = if machine.jobs.is_empty() {
                "idle".to_owned()
            } else {
                machine.jobs.join(", ")
            };
            row![
                text(machine.name.as_str()).size(14).width(160),
                text(machine.kind.as_str()).size(12).color(muted).width(110),
                text(machine.status.as_str()).size(12).width(150),
                text(jobs).size(12),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        });
        scrollable(column(rows).spacing(12).padding(16))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// A channel: its toolbar over the list of its pipelines.
    fn channel_view<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
    ) -> Element<'a, Message, Theme> {
        let secondary = |theme: &Theme, status| button::secondary(theme.base(), status);
        let mut toolbar = row![text(state.title.as_str()).size(TEXT_MD)]
            .spacing(SP_3)
            .align_y(Alignment::Center)
            .padding([SP_2, SP_4]);
        if let Some(open) = &state.transcript {
            toolbar = toolbar
                .push(
                    text(format!("#{} {}", open.number, open.job))
                        .size(TEXT_SM)
                        .color(self.theme.muted()),
                )
                .push(
                    button(text("Close transcript").size(TEXT_SM))
                        .padding([SP_1, SP_2])
                        .style(secondary)
                        .on_press_with(move || {
                            Message::Ci(runner.clone(), CiMsg::CloseTranscript { view })
                        }),
                );
            if let Some(error) = &open.error {
                toolbar =
                    toolbar.push(text(error.as_str()).size(TEXT_SM).color(self.theme.error()));
            }
        }
        if !state.pipelines.is_empty()
            && let Some(error) = &state.error
        {
            toolbar = toolbar.push(text(error.as_str()).size(TEXT_SM).color(self.theme.error()));
        }

        let body: Element<'a, Message, Theme> = if state.pipelines.is_empty() {
            unavailable(
                &state.title,
                state.error.as_deref().unwrap_or("Loading pipelines."),
            )
        } else {
            self.pipeline_list(runner, view, state)
        };
        column![toolbar, rule::horizontal(1), body].into()
    }

    /// The pipelines of a channel, newest on top and separated by rules, and
    /// at the end the button that loads older ones. The list scrolls both
    /// ways: a pipeline's drawing is as wide as its jobs need, and the list
    /// is never narrower than the pane, so the rules reach across it.
    fn pipeline_list<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        state: &'a ChannelState,
    ) -> Element<'a, Message, Theme> {
        let widest = state
            .pipelines
            .iter()
            .map(|shown| shown.jobs.size.width)
            .fold(0.0, f32::max)
            + 2.0 * SP_4;
        responsive(move |size| {
            let mut list = column![]
                .spacing(SP_4)
                .padding(SP_4)
                .width(widest.max(size.width));
            for (at, shown) in state.pipelines.iter().enumerate() {
                if at > 0 {
                    list = list.push(rule::horizontal(1));
                }
                list = list.push(self.pipeline_entry(runner, view, shown));
            }
            if state.loaded && !state.exhausted {
                let older = button(text("Load older").size(TEXT_SM))
                    .padding([SP_1, SP_2])
                    .style(|theme: &Theme, status| button::secondary(theme.base(), status));
                list = list.push(if state.loading {
                    older
                } else {
                    older.on_press_with(move || {
                        Message::Ci(runner.clone(), CiMsg::LoadOlder { view })
                    })
                });
            }
            scrollable(list)
                .direction(scrollable::Direction::Both {
                    vertical: scrollable::Scrollbar::default(),
                    horizontal: scrollable::Scrollbar::default(),
                })
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        })
        .into()
    }

    /// One pipeline: what started it and how it went, the commit's subject,
    /// the pipeline's note, and the drawing of its jobs.
    fn pipeline_entry<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        shown: &'a Shown,
    ) -> Element<'a, Message, Theme> {
        let pipeline = &shown.pipeline;
        let tone = Tone::of(&pipeline.status);
        let muted = self.theme.muted();
        let status = match pipeline.finished {
            Some(finished) if finished >= pipeline.created => {
                format!(
                    "{} in {}",
                    pipeline.status,
                    duration(finished - pipeline.created)
                )
            }
            _ => pipeline.status.clone(),
        };
        let mut header = column![
            row![
                text(summary(pipeline)).size(TEXT_MD),
                text(status).size(TEXT_SM).color(tone.text(&self.theme)),
            ]
            .spacing(SP_3)
            .align_y(Alignment::Center)
        ]
        .spacing(SP_1);
        if !pipeline.subject.is_empty() {
            header = header.push(
                text(flatten(&pipeline.subject, usize::MAX))
                    .size(TEXT_MD)
                    .color(muted),
            );
        }
        if !pipeline.note.is_empty() {
            let color = match tone {
                Tone::Bad => self.theme.error(),
                _ => muted,
            };
            header = header.push(
                text(flatten(&pipeline.note, usize::MAX))
                    .size(TEXT_SM)
                    .color(color),
            );
        }
        let mut entry = column![header].spacing(SP_3);
        if !shown.jobs.cards.is_empty() {
            entry = entry.push(self.job_graph(runner, view, shown));
        }
        entry.into()
    }

    /// A pipeline's jobs: the cables at the bottom, a card per job over
    /// them. Only the cards take input.
    fn job_graph<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        shown: &'a Shown,
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
            layers.push(
                pin(self.job_card(runner, view, number, job))
                    .position(at)
                    .into(),
            );
        }
        stack(layers)
            .width(graph.size.width)
            .height(graph.size.height)
            .into()
    }

    /// A job's card: its name, how its run went, where it runs and its note,
    /// outlined by how the run went. Pressing it opens the job's transcript.
    fn job_card<'a>(
        &'a self,
        runner: &'a RunnerKey,
        view: u64,
        number: u64,
        job: &'a JobView,
    ) -> Element<'a, Message, Theme> {
        let muted = self.theme.muted();
        let tone = Tone::of(&job.status);
        let status = match (job.started, job.finished) {
            (Some(started), Some(finished)) if finished >= started => {
                format!("{} {}", job.status, duration(finished - started))
            }
            _ => job.status.clone(),
        };
        let mut lines = column![
            text(job.name.as_str())
                .size(TEXT_MD)
                .wrapping(Wrapping::None),
            text(status)
                .size(TEXT_SM)
                .color(tone.text(&self.theme))
                .wrapping(Wrapping::None),
            text(job.place.as_str())
                .size(TEXT_SM)
                .color(muted)
                .wrapping(Wrapping::None),
        ]
        .spacing(SP_1);
        if !job.note.is_empty() {
            lines = lines.push(
                text(flatten(&job.note, NOTE_CHARS))
                    .size(TEXT_SM)
                    .color(muted),
            );
        }
        button(lines)
            .width(JOB_WIDTH)
            .height(JOB_HEIGHT)
            .padding(SP_2)
            .clip(true)
            .style(move |theme: &Theme, status| tone.card(theme, status))
            .on_press_with(move || {
                Message::Ci(
                    runner.clone(),
                    CiMsg::OpenJob {
                        view,
                        number,
                        job: job.name.clone(),
                    },
                )
            })
            .into()
    }
}

/// How a run went: the colour of a job card's outline and of a status word.
#[derive(Clone, Copy)]
enum Tone {
    Good,
    Bad,
    Active,
    Idle,
}

impl Tone {
    fn of(status: &str) -> Tone {
        match status {
            "succeeded" => Tone::Good,
            "failed" | "error" => Tone::Bad,
            "running" | "starting" => Tone::Active,
            _ => Tone::Idle,
        }
    }

    fn color(self, theme: &Theme) -> Color {
        let palette = theme.extended();
        match self {
            Tone::Good => palette.success.base.color,
            Tone::Bad => palette.danger.base.color,
            Tone::Active => palette.primary.base.color,
            Tone::Idle => palette.background.strong.color,
        }
    }

    /// A status word: the outline's hue mixed towards the foreground, since
    /// a border colour is too dark to read as text on a dark theme; an idle
    /// one is plain secondary text.
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
    /// step lighter under the pointer so it reads as pressable.
    fn card(self, theme: &Theme, status: button::Status) -> button::Style {
        let palette = theme.extended();
        let background = match status {
            button::Status::Hovered => palette.background.weak.color,
            button::Status::Pressed => palette.background.neutral.color,
            button::Status::Active | button::Status::Disabled => palette.background.weaker.color,
        };
        button::Style {
            background: Some(background.into()),
            text_color: palette.background.base.text,
            border: iced::Border {
                color: self.color(theme),
                width: 2.0,
                radius: 8.0.into(),
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
