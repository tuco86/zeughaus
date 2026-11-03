//! The runners' terminal multiplexers as this editor holds them: one
//! attachment per runner, one view per terminal, and the pane that draws it.
//!
//! Native only: a terminal is a PTY on a runner reached over QUIC, and the
//! browser editor has neither.

use std::collections::HashMap;
use std::sync::Arc;

use iced::widget::container;
use iced::{Element, Task};
use zeughaus_theme::Theme;

use super::{App, unavailable};
use crate::message::Message;
use crate::mux::{self, MuxEvent, TerminalAction};
use crate::workspace::{PaneRef, RunnerKey, Surface};

/// The terminal's font size in logical pixels: 12 pt at 96 dpi, what a
/// desktop terminal defaults to. One size for every pane: the cell grid is
/// measured from it, and two panes at different sizes would report
/// different geometries for the same terminal.
const TERMINAL_FONT_SIZE: f32 = 16.0;

/// Logical pixels between a terminal's grid and the pane's left and right
/// edges, so the first and last column do not touch the pane border.
const TERMINAL_PADDING: f32 = 8.0;
/// Logical pixels above and below the grid, in the terminal's background,
/// so the first row does not sit on the tab bar's edge.
const TERMINAL_PADDING_Y: f32 = 4.0;

/// Everything this editor holds about one runner's terminal multiplexer.
///
/// Replaced wholesale when the runner's endpoint changes: a different runner
/// is a different set of terminals, and nothing cached about the old one
/// means anything to the new one. The control task's handle aborts on drop,
/// so dropping this is also how the attachment is given up.
pub(super) struct MuxState {
    /// Which attachment the events on screen came from, drawn from the
    /// editor-wide epoch counter: a message the replaced task queued names
    /// no live mux.
    epoch: u64,
    /// This editor process, as the runner names it when it hands out a
    /// control lease.
    client: zeughaus_mux::ClientInstanceId,
    /// Sends topology commands, once the control task has attached far
    /// enough to hand one out.
    commands: Option<crate::mux::CommandSender>,
    /// Which runner process this is. A different one on attach means every
    /// cached terminal belongs to terminals that no longer exist.
    incarnation: Option<zeughaus_mux::RunnerIncarnation>,
    /// How the runner names this editor's identity to the others. A lease
    /// held by another instance under the same principal -- this user's
    /// previous editor -- is one this editor may type through.
    principal: Option<String>,
    /// The protocol minor both ends speak: the lower of the two.
    pub minor: u8,
    /// Whether the last thing the control task said was an attach. Drives
    /// the status bar and decides whether a structural command may be sent.
    pub attached: bool,
    workspace: Option<zeughaus_mux::WorkspaceSnapshot>,
    terminals: HashMap<zeughaus_mux::TerminalId, LiveTerminal>,
    /// Commands sent and not yet answered, by their correlation id. Resent
    /// with the same id after a reattach, which the runner deduplicates, so a
    /// split made while the network blinked happens once rather than twice or
    /// not at all.
    pending: HashMap<zeughaus_mux::RequestId, zeughaus_mux::TopologyCommand>,
    next_request: u64,
    _control: iced::task::Handle,
}

/// One terminal as this editor holds it: the screen, the way to type into it,
/// and the task carrying both.
pub(super) struct LiveTerminal {
    /// Written by the terminal's task, read by the pane. `None` inside
    /// until the first head arrives; a pane with no view yet draws its
    /// placeholder rather than nothing.
    view: crate::mux::SharedView,
    commands: Option<crate::mux::TerminalSender>,
    serials: crate::mux::Serials,
    /// Aborts on drop, so removing the entry stops the stream. `None` while
    /// the connection is down: the view stays on screen, the task does not.
    task: Option<iced::task::Handle>,
}

impl LiveTerminal {
    fn detached() -> LiveTerminal {
        LiveTerminal {
            view: Arc::new(std::sync::Mutex::new(None)),
            commands: None,
            serials: crate::mux::Serials::default(),
            task: None,
        }
    }

    /// The epoch and sequence the view holds, for an attach that continues
    /// with deltas, and the size it was drawn at.
    fn known(&self) -> (Option<(u64, u64)>, zeughaus_mux::Dimensions) {
        let guard = self.view.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(view) => (Some((view.epoch, view.applied_seq)), view.dimensions),
            None => (None, mux::DEFAULT_GRID),
        }
    }
}

impl App {
    /// Whether the user is looking at `terminal` of runner `key`: the
    /// window has the keyboard, no CI page covers the workspace, and the
    /// focused pane shows it.
    pub(super) fn watched(&self, key: &RunnerKey, terminal: zeughaus_mux::TerminalId) -> bool {
        self.window_focused
            && !self.page.open
            && self.workspace.focused_pane().is_some_and(|pane| {
                pane.runner == *key
                    && self.workspace.surface_of(pane) == Some(Surface::Terminal(terminal))
            })
    }

    /// Replaces `key`'s control task with one for its current endpoint.
    ///
    /// Terminal views are dropped with the old one -- a terminal id is the
    /// old runner's, and the new one will hand out its own.
    pub(super) fn restart_mux(&mut self, key: &RunnerKey) -> Task<Message> {
        self.mux.remove(key);
        let Some(endpoint) = self.runtime.links.get(key).map(|l| l.endpoint.clone()) else {
            // Nothing serves a shell: the section keeps no tabs.
            self.workspace.detach(key);
            self.rebuild_palette();
            return Task::none();
        };
        self.runtime.next_epoch += 1;
        let epoch = self.runtime.next_epoch;
        let (task, handle) = Task::run(mux::control(endpoint), move |event| {
            Message::Mux(epoch, event)
        })
        .abortable();
        self.mux.insert(
            key.clone(),
            MuxState {
                epoch,
                client: mux::client_instance(),
                commands: None,
                incarnation: None,
                principal: None,
                minor: 0,
                attached: false,
                workspace: None,
                terminals: HashMap::new(),
                pending: HashMap::new(),
                next_request: 0,
                _control: handle.abort_on_drop(),
            },
        );
        // What the palette offers about a runner depends on there being one.
        self.rebuild_palette();
        task
    }

    /// Whether `key` is a CI service link rather than a runner.
    pub(super) fn is_ci_link(&self, key: &RunnerKey) -> bool {
        self.runtime
            .links
            .get(key)
            .is_some_and(|link| link.kind == super::runtime::LinkKind::Ci)
    }

    /// The runner whose mux attachment `epoch` names, if it is still live.
    fn mux_key(&self, epoch: u64) -> Option<RunnerKey> {
        self.mux
            .iter()
            .find(|(_, mux)| mux.epoch == epoch)
            .map(|(key, _)| key.clone())
    }

    /// What a control task said.
    pub(super) fn apply_mux(&mut self, epoch: u64, event: MuxEvent) -> Task<Message> {
        let Some(key) = self.mux_key(epoch) else {
            return Task::none();
        };
        match event {
            MuxEvent::Ready(sender) => {
                if let Some(mux) = self.mux.get_mut(&key) {
                    mux.commands = Some(sender);
                }
                Task::none()
            }
            MuxEvent::Attached {
                hello,
                workspace,
                heads,
            } => {
                let Some(mux) = self.mux.get_mut(&key) else {
                    return Task::none();
                };
                // A different runner process owns different terminals: every
                // id this editor holds names something that no longer exists.
                let restarted = mux
                    .incarnation
                    .is_some_and(|held| held != hello.incarnation);
                if restarted {
                    mux.terminals.clear();
                }
                mux.incarnation = Some(hello.incarnation);
                mux.minor = hello.minor;
                mux.principal = Some(hello.principal);
                mux.attached = true;
                for head in heads {
                    let live = mux
                        .terminals
                        .entry(head.terminal)
                        .or_insert_with(LiveTerminal::detached);
                    mux::apply_head(&live.view, head);
                }
                mux.workspace = Some((*workspace).clone());
                // Sent before the blink and unanswered: the runner
                // deduplicates by request id, so this applies at most once.
                let resend: Vec<zeughaus_mux::message::Command> = mux
                    .pending
                    .iter()
                    .map(|(request, command)| zeughaus_mux::message::Command {
                        request: *request,
                        command: command.clone(),
                    })
                    .collect();
                if let Some(sender) = mux.commands.as_mut() {
                    for command in resend {
                        let _ = sender.try_send(command);
                    }
                }
                // A CI has no workspace: its sections are drawn from its CI
                // client, and its empty snapshot must not create a runner
                // section under its key.
                if !self.is_ci_link(&key) {
                    self.workspace.apply_snapshot(&key, *workspace);
                }
                // The transcripts a CI view showed were that process's
                // terminals too.
                let reopen = if restarted {
                    self.reopen_ci_transcripts(&key)
                } else {
                    Task::none()
                };
                self.rebuild_palette();
                Task::batch([reopen, self.reconcile_terminals(&key)])
            }
            MuxEvent::Workspace(snapshot) => {
                let Some(mux) = self.mux.get_mut(&key) else {
                    return Task::none();
                };
                mux.workspace = Some((*snapshot).clone());
                self.workspace.apply_snapshot(&key, *snapshot);
                self.rebuild_palette();
                self.reconcile_terminals(&key)
            }
            MuxEvent::Reply(reply) => {
                let Some(mux) = self.mux.get_mut(&key) else {
                    return Task::none();
                };
                mux.pending.remove(&reply.request);
                if let zeughaus_mux::CommandOutcome::Refused { reason } = reply.outcome {
                    self.hint = Some((reason, iced::time::Instant::now()));
                    // What this window waited for is not coming; a stale
                    // wait would pull a later, unrelated tab to the front.
                    self.pending_graph_focus = None;
                    self.workspace.forget_expected(&key);
                }
                Task::none()
            }
            MuxEvent::Lost => {
                let Some(mux) = self.mux.get_mut(&key) else {
                    return Task::none();
                };
                mux.attached = false;
                // The screens stay on display as last-known; their streams
                // belong to a connection that no longer exists.
                for live in mux.terminals.values_mut() {
                    live.task = None;
                    live.commands = None;
                }
                Task::none()
            }
            MuxEvent::GaveUp => {
                // This address names a peer that is gone for good. The endpoint
                // file (or `zeughaus.toml`) names the replacement, and reconciliation dials it.
                self.mux.remove(&key);
                self.workspace.detach(&key);
                self.rebuild_palette();
                Task::none()
            }
        }
    }

    /// Brings `key`'s terminal streams in line with what its workspace shows.
    ///
    /// One task per terminal any tab references or a CI view shows as a
    /// transcript; nothing for a terminal that was closed, and no second task
    /// for one already streaming. A terminal whose view survived a blink
    /// reattaches at the sequence it holds, so the runner continues with
    /// deltas instead of resending the screen.
    pub(super) fn reconcile_terminals(&mut self, key: &RunnerKey) -> Task<Message> {
        let Some(endpoint) = self.runtime.links.get(key).map(|l| l.endpoint.clone()) else {
            return Task::none();
        };
        // The transcripts a CI view of this runner shows and the terminals
        // the CI page docks are its terminals too, though no pane of its
        // workspace names them.
        let mut replays: Vec<zeughaus_mux::TerminalId> = self
            .ci
            .get(key)
            .map(|client| client.terminals().collect())
            .unwrap_or_default();
        replays.extend(self.page_terminals(key));
        let Some(mux) = self.mux.get_mut(key) else {
            return Task::none();
        };
        let epoch = mux.epoch;
        let wanted: Vec<zeughaus_mux::TerminalId> = mux
            .workspace
            .iter()
            .flat_map(|workspace| workspace.terminals())
            .chain(replays)
            .collect();
        // Removal is the whole lifetime: the handle aborts on drop, so a
        // closed pane's stream cannot outlive it.
        mux.terminals.retain(|id, _| wanted.contains(id));
        let client = mux.client;
        let mut tasks = Vec::new();
        for terminal in wanted {
            let live = mux
                .terminals
                .entry(terminal)
                .or_insert_with(LiveTerminal::detached);
            if live.task.is_some() {
                continue;
            }
            let (known, size) = live.known();
            let attach = zeughaus_mux::TerminalAttach {
                client,
                terminal,
                known,
                size,
            };
            let (task, handle) = Task::run(
                mux::terminal(endpoint.clone(), attach, Arc::clone(&live.view)),
                move |event| Message::Terminal(epoch, terminal, event),
            )
            .abortable();
            live.task = Some(handle.abort_on_drop());
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    /// What one terminal's stream said. The rows themselves never come this
    /// way: the task wrote them into the shared view, and `Changed` is the
    /// redraw this message already causes.
    pub(super) fn apply_terminal(
        &mut self,
        epoch: u64,
        terminal: zeughaus_mux::TerminalId,
        event: crate::mux::TerminalEvent,
    ) -> Task<Message> {
        use crate::mux::TerminalEvent;

        let Some(key) = self.mux_key(epoch) else {
            return Task::none();
        };
        let Some(live) = self
            .mux
            .get_mut(&key)
            .and_then(|mux| mux.terminals.get_mut(&terminal))
        else {
            return Task::none();
        };
        match event {
            TerminalEvent::Ready(sender) => live.commands = Some(sender),
            // The task already wrote the change into the view; what is left
            // is fetching the pixels of images it newly shows.
            TerminalEvent::Changed => return self.fetch_images(&key, terminal),
            TerminalEvent::Notification { title, body } => {
                let summary = title
                    .or_else(|| {
                        let guard = live.view.lock().unwrap_or_else(|e| e.into_inner());
                        guard
                            .as_ref()
                            .map(|view| view.title.clone())
                            .filter(|t| !t.is_empty())
                    })
                    .unwrap_or_else(|| "Zeughaus".to_owned());
                // A replay says what the job said when it ran, which is not
                // news.
                if self
                    .ci
                    .get(&key)
                    .is_some_and(|client| client.shows(terminal))
                {
                    return Task::none();
                }
                // An agent's own "waiting for input" says what its status
                // already does, as a notice of its own.
                let agent = self.runtime.activity.get(&key).is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| e.terminal == terminal.0 && e.agent.is_some())
                });
                // When the user is looking at it, the terminal already shows
                // whatever the child wanted to say.
                if !agent && !self.watched(&key, terminal) {
                    self.raise(
                        None,
                        crate::notices::Severity::Info,
                        summary,
                        body,
                        false,
                        Some(crate::message::NoticeTarget::Terminal(key, terminal)),
                    );
                }
            }
            // Only the editor whose user picked the link opens it: the one
            // driving the terminal, or without a driver the one looking at it.
            TerminalEvent::ShowTicket { repo, number } => {
                let controller = {
                    let guard = live.view.lock().unwrap_or_else(|e| e.into_inner());
                    guard.as_ref().and_then(|view| view.controller.clone())
                };
                let ours = match controller {
                    Some(controller) => controller.client == mux::client_instance(),
                    None => self.watched(&key, terminal),
                };
                if ours {
                    return self.show_ticket(&repo, number);
                }
            }
            // The task cleared the view and ended; attaching again asks for
            // a fresh head.
            TerminalEvent::Desynced => {
                live.commands = None;
                live.task = None;
                return self.reconcile_terminals(&key);
            }
            TerminalEvent::Error(error) => {
                eprintln!("[mux] {terminal}: {}", error.message);
                self.hint = Some((error.message, iced::time::Instant::now()));
            }
            // The runner ended this stream. The pane keeps its last screen;
            // the workspace snapshot is what removes it.
            TerminalEvent::Ended => {
                live.commands = None;
                live.task = None;
            }
        }
        Task::none()
    }

    /// A scrollback page, or why it did not arrive.
    pub(super) fn apply_row_page(
        &mut self,
        epoch: u64,
        terminal: zeughaus_mux::TerminalId,
        page: Result<zeughaus_mux::RowPage, String>,
    ) -> Task<Message> {
        let Some(key) = self.mux_key(epoch) else {
            return Task::none();
        };
        let page = match page {
            Ok(page) => page,
            Err(e) => {
                eprintln!("[mux] {terminal}: {e}");
                return Task::none();
            }
        };
        if let Some(live) = self
            .mux
            .get_mut(&key)
            .and_then(|mux| mux.terminals.get_mut(&terminal))
            && let Some(view) = live.view.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            view.apply_page(page);
        }
        self.fetch_images(&key, terminal)
    }

    /// Fetches the pixels of images the terminal's viewport shows and its
    /// view neither holds nor has asked for.
    fn fetch_images(
        &mut self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
    ) -> Task<Message> {
        let Some(endpoint) = self.runtime.links.get(key).map(|l| l.endpoint.clone()) else {
            return Task::none();
        };
        let Some(mux) = self.mux.get(key) else {
            return Task::none();
        };
        let epoch = mux.epoch;
        let Some(live) = mux.terminals.get(&terminal) else {
            return Task::none();
        };
        let missing = match live.view.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(view) => view.missing_images(),
            None => return Task::none(),
        };
        Task::batch(missing.into_iter().map(|image| {
            Task::perform(
                mux::fetch_resource(
                    endpoint.clone(),
                    zeughaus_mux::ResourceFetch { terminal, image },
                ),
                move |resource| Message::Resource(epoch, terminal, image, resource),
            )
        }))
    }

    /// An image's pixels, or why they did not arrive.
    pub(super) fn apply_resource(
        &mut self,
        epoch: u64,
        terminal: zeughaus_mux::TerminalId,
        image: zeughaus_mux::ImageKey,
        resource: Result<zeughaus_mux::Resource, String>,
    ) {
        let Some(key) = self.mux_key(epoch) else {
            return;
        };
        let Some(live) = self
            .mux
            .get_mut(&key)
            .and_then(|mux| mux.terminals.get_mut(&terminal))
        else {
            return;
        };
        let mut guard = live.view.lock().unwrap_or_else(|e| e.into_inner());
        let Some(view) = guard.as_mut() else {
            return;
        };
        match resource {
            Ok(resource) if resource.image == image => view.apply_image(image, resource.pixels),
            Ok(_) => view.forget_image_request(image),
            Err(e) => {
                eprintln!("[mux] {terminal}: {e}");
                view.forget_image_request(image);
            }
        }
    }

    /// What a terminal pane reported the user did.
    pub(super) fn apply_terminal_action(
        &mut self,
        pane: PaneRef,
        action: TerminalAction,
    ) -> Task<Message> {
        let Some(Surface::Terminal(terminal)) = self.workspace.surface_of(&pane) else {
            return Task::none();
        };
        let key = pane.runner.clone();
        self.terminal_action(&key, terminal, action, Some(pane))
    }

    /// What a terminal the CI page docks reported the user did. It is looked
    /// at, not driven: only its size follows the dock.
    pub(super) fn apply_page_terminal_action(
        &mut self,
        key: RunnerKey,
        terminal: zeughaus_mux::TerminalId,
        action: TerminalAction,
    ) -> Task<Message> {
        self.terminal_action(&key, terminal, action, None)
    }

    /// One terminal's action; without a pane it is read-only.
    fn terminal_action(
        &mut self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
        action: TerminalAction,
        pane: Option<PaneRef>,
    ) -> Task<Message> {
        let read_only = pane.is_none();
        match action {
            TerminalAction::Command(command @ zeughaus_mux::TerminalCommand::Resize { .. }) => {
                self.send_input(key, terminal, command);
                Task::none()
            }
            TerminalAction::Command(_) | TerminalAction::TakeControl if read_only => Task::none(),
            TerminalAction::Command(command) => {
                self.send_input(key, terminal, command);
                Task::none()
            }
            TerminalAction::ScrollBy(lines) => self.scroll_terminal(key, terminal, lines),
            TerminalAction::Copy(text) => iced::clipboard::write(text),
            TerminalAction::OpenLink(url) => {
                // Only a Ctrl+click on the link gets here.
                if let Err(e) = open_link(&url) {
                    eprintln!("[mux] link {url}: {e}");
                    self.hint = Some((format!("{url}: {e}"), iced::time::Instant::now()));
                }
                Task::none()
            }
            TerminalAction::TakeControl => {
                self.send_topology(key, zeughaus_mux::TopologyCommand::TakeControl { terminal });
                Task::none()
            }
            TerminalAction::Focused(focused) => {
                if focused && let Some(pane) = pane {
                    self.workspace.focus(pane);
                }
                Task::none()
            }
        }
    }

    /// Sends one input command, remembering the serial it went out with.
    pub(super) fn send_input(
        &mut self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
        command: zeughaus_mux::TerminalCommand,
    ) {
        let Some(live) = self
            .mux
            .get_mut(key)
            .and_then(|mux| mux.terminals.get_mut(&terminal))
        else {
            return;
        };
        let serial = command.serial();
        let Some(sender) = live.commands.as_mut() else {
            // Nothing is connected: input is discarded rather than queued.
            // A keystroke replayed after a resync would be typed into a
            // screen that has moved on.
            return;
        };
        if sender.try_send(command).is_err() {
            return;
        }
        if let Some(serial) = serial {
            live.serials.record(serial);
        }
    }

    /// Scrolls one terminal's viewport and tells the runner which rows this
    /// client now watches, fetching the ones it does not hold.
    pub(super) fn scroll_terminal(
        &mut self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
        lines: i64,
    ) -> Task<Message> {
        let Some(endpoint) = self.runtime.links.get(key).map(|l| l.endpoint.clone()) else {
            return Task::none();
        };
        let Some(mux) = self.mux.get_mut(key) else {
            return Task::none();
        };
        let epoch = mux.epoch;
        let Some(live) = mux.terminals.get_mut(&terminal) else {
            return Task::none();
        };
        // The lock covers the scroll and the bookkeeping, not the fetch
        // tasks: they are only built here and run later.
        let mut guard = live.view.lock().unwrap_or_else(|e| e.into_inner());
        let Some(view) = guard.as_mut() else {
            return Task::none();
        };
        if !view.scroll_by(lines) {
            return Task::none();
        }
        let viewport = view.viewport();
        if let Some(sender) = live.commands.as_mut() {
            let _ = sender.try_send(zeughaus_mux::TerminalCommand::Viewport {
                first_row: viewport.start,
                rows: view.dimensions.rows,
            });
        }
        let view_epoch = view.epoch;
        let mut tasks = Vec::new();
        for range in view.missing_rows(viewport) {
            // One fetch cannot ask for more than the protocol allows, and a
            // viewport is never that tall anyway.
            let range = zeughaus_mux::StableRange {
                start: range.start,
                end: range
                    .end
                    .min(range.start + zeughaus_mux::message::MAX_FETCH_ROWS as i64),
            };
            let fetch = zeughaus_mux::RowFetch {
                terminal,
                epoch: view_epoch,
                range,
                generation: view.next_fetch_generation(),
            };
            tasks.push(Task::perform(
                mux::fetch(endpoint.clone(), fetch),
                move |page| Message::RowPage(epoch, terminal, page),
            ));
        }
        drop(guard);
        tasks.push(self.fetch_images(key, terminal));
        Task::batch(tasks)
    }

    /// Sends a structural change to `key`'s runner, or says why it cannot.
    pub(super) fn send_topology(
        &mut self,
        key: &RunnerKey,
        command: zeughaus_mux::TopologyCommand,
    ) {
        let refusal = match self.mux.get_mut(key) {
            None if key.is_synthetic() => Some("no runner executes these graphs"),
            None => Some("no mux on this runner: its workspace cannot be changed"),
            Some(mux) if !mux.attached => Some("reconnecting: the change was not sent"),
            Some(mux) => {
                mux.next_request += 1;
                let request = zeughaus_mux::RequestId(mux.next_request);
                let wire = zeughaus_mux::message::Command {
                    request,
                    command: command.clone(),
                };
                match mux.commands.as_mut().map(|sender| sender.try_send(wire)) {
                    Some(Ok(())) => {
                        mux.pending.insert(request, command);
                        None
                    }
                    Some(Err(_)) | None => Some("the runner is not taking commands"),
                }
            }
        };
        if let Some(refusal) = refusal {
            self.hint = Some((refusal.to_owned(), iced::time::Instant::now()));
        }
    }

    /// The terminal a pane shows, as this editor holds it, with the serials
    /// its input carries. `None` while no head has arrived.
    pub(super) fn terminal_view(
        &self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
    ) -> Option<(&mux::SharedView, mux::Serials)> {
        let live = self.mux.get(key)?.terminals.get(&terminal)?;
        let present = live
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        present.then_some((&live.view, live.serials))
    }

    /// Terminals this window has scrolled back, with how many rows above the
    /// live screen their viewport starts: what a restart hands on.
    #[cfg(unix)]
    pub(super) fn terminal_scrolls(&self) -> Vec<(RunnerKey, zeughaus_mux::TerminalId, i64)> {
        let mut scrolls: Vec<_> = self
            .mux
            .iter()
            .flat_map(|(key, mux)| {
                mux.terminals.iter().filter_map(move |(terminal, live)| {
                    let guard = live.view.lock().unwrap_or_else(|e| e.into_inner());
                    let view = guard.as_ref()?;
                    let top = view.scroll_top?;
                    Some((key.clone(), *terminal, view.visible.start - top))
                })
            })
            .collect();
        scrolls.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        scrolls
    }

    /// Whether `terminal`'s first screen has arrived, so it can scroll.
    pub(super) fn terminal_has_screen(
        &self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
    ) -> bool {
        self.terminal_view(key, terminal).is_some()
    }

    /// One terminal pane.
    ///
    /// The terminal itself: one widget drawing the view, with the keyboard
    /// when its pane is focused and the runner's lease when this client
    /// holds it -- or when nobody does: the first client that types acquires
    /// an unclaimed terminal, so its keys must go out. A viewer sees the same
    /// rows and gets the take-control shortcut instead of the keys. A
    /// `read_only` pane is never the controlling one, whoever holds the
    /// lease: a transcript is looked at, not driven, and only sized.
    pub(super) fn terminal_pane(
        &self,
        pane: Option<PaneRef>,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
        read_only: bool,
    ) -> Element<'_, Message, Theme> {
        // The palette or a name field takes the keyboard while it is open; a
        // shell that kept it would receive everything typed there.
        let focused = !self.palette_open
            && self.workspace.renaming_group.is_none()
            && self.workspace.renaming_tab.is_none()
            && pane.is_some()
            && pane.as_ref() == self.workspace.focused_pane();
        let on_action = pane.map(|pane| {
            Box::new(move |action| Message::TerminalAction(pane.clone(), action))
                as Box<dyn Fn(TerminalAction) -> Message>
        });
        self.terminal_widget(key, terminal, read_only, focused, on_action)
    }

    /// A terminal the CI page docks: read-only, sized by the dock and
    /// scrolled there.
    pub(super) fn page_terminal(
        &self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
    ) -> Element<'_, Message, Theme> {
        let runner = key.clone();
        let on_action: Box<dyn Fn(TerminalAction) -> Message> =
            Box::new(move |action| Message::PageTerminal(runner.clone(), terminal, action));
        self.terminal_widget(key, terminal, true, false, Some(on_action))
    }

    fn terminal_widget<'a>(
        &'a self,
        key: &RunnerKey,
        terminal: zeughaus_mux::TerminalId,
        read_only: bool,
        focused: bool,
        on_action: Option<Box<dyn Fn(TerminalAction) -> Message>>,
    ) -> Element<'a, Message, Theme> {
        let Some((view, serials)) = self.terminal_view(key, terminal) else {
            return unavailable("Terminal", "Waiting for the runner's first screen.");
        };
        let principal = self.mux.get(key).and_then(|mux| mux.principal.as_deref());
        let controlling = !read_only && {
            let guard = view.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().is_none_or(|view| {
                view.controller.as_ref().is_none_or(|controller| {
                    controller.client == mux::client_instance()
                        || principal == Some(controller.principal.as_str())
                })
            })
        };
        let widget = iced_terminal::Terminal::new(Arc::clone(view), terminal.0)
            .controlling(controlling)
            .sizing(read_only)
            .focused(focused)
            .reserved(super::is_app_shortcut)
            .next_serial(serials.next())
            .font_size(TERMINAL_FONT_SIZE)
            .padding(TERMINAL_PADDING)
            .scale_factor(self.scale_factor);
        let widget: Element<'a, Message, Theme> = match on_action {
            None => widget.into(),
            Some(on_action) => widget.on_action(on_action).into(),
        };
        let background = self.theme.scheme().background;
        container(widget)
            .padding([TERMINAL_PADDING_Y, 0.0])
            .style(move |_: &Theme| container::Style {
                background: Some(background.into()),
                ..container::Style::default()
            })
            .into()
    }
}

/// Opens a link the user activated with the platform's handler. Only schemes
/// that name something to show: a link from a terminal or a document must
/// not become an arbitrary launcher invocation.
pub(super) fn open_link(url: &str) -> Result<(), String> {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme);
    match scheme.map(str::to_ascii_lowercase).as_deref() {
        Some("http" | "https" | "file") => open::that_detached(url).map_err(|e| e.to_string()),
        _ => Err("only http, https and file links are opened".to_owned()),
    }
}
