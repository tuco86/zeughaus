//! The runner's terminal multiplexer as this editor holds it: one attachment,
//! one view per terminal, and the pane that draws it.
//!
//! Native only: a terminal is a PTY on the runner reached over QUIC, and the
//! browser editor has neither.

use std::collections::HashMap;
use std::sync::Arc;

use iced::{Element, Task};

use super::{App, unavailable};
use crate::message::Message;
use crate::mux::{self, MuxEvent, TerminalAction};
use crate::workspace::{Surface, surface_title};

/// The terminal's font size in logical pixels: 12 pt at 96 dpi, what a
/// desktop terminal defaults to. One size for every pane: the cell grid is
/// measured from it, and two panes at different sizes would report
/// different geometries for the same terminal.
const TERMINAL_FONT_SIZE: f32 = 16.0;

/// Everything this editor holds about one runner's terminal multiplexer.
///
/// Replaced wholesale when the runner endpoint changes: a different runner is
/// a different set of terminals, and nothing cached about the old one means
/// anything to the new one. The control task's handle aborts on drop, so
/// dropping this is also how the attachment is given up.
pub(super) struct MuxState {
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
    /// Replaces the control task with one for the current endpoint.
    ///
    /// The epoch is bumped first: whatever the old task still has queued
    /// describes a runner this editor no longer talks to. Terminal views are
    /// dropped with it -- a terminal id is the old runner's, and the new one
    /// will hand out its own.
    pub(super) fn restart_mux(&mut self) -> Task<Message> {
        self.mux = None;
        self.mux_epoch += 1;
        let Some(endpoint) = self.runtime.endpoint.clone() else {
            // Nothing serves a shell: back to the workspace an editor has on
            // its own, which is the graph and nothing else.
            self.workspace.detach();
            return Task::none();
        };
        let epoch = self.mux_epoch;
        let (task, handle) = Task::run(mux::control(endpoint), move |event| {
            Message::Mux(epoch, event)
        })
        .abortable();
        self.mux = Some(MuxState {
            client: mux::client_instance(),
            commands: None,
            incarnation: None,
            principal: None,
            attached: false,
            workspace: None,
            terminals: HashMap::new(),
            pending: HashMap::new(),
            next_request: 0,
            _control: handle.abort_on_drop(),
        });
        task
    }

    /// What the control task said.
    pub(super) fn apply_mux(&mut self, epoch: u64, event: MuxEvent) -> Task<Message> {
        if epoch != self.mux_epoch {
            return Task::none();
        }
        match event {
            MuxEvent::Ready(sender) => {
                if let Some(mux) = self.mux.as_mut() {
                    mux.commands = Some(sender);
                }
                Task::none()
            }
            MuxEvent::Attached {
                hello,
                workspace,
                heads,
            } => {
                let Some(mux) = self.mux.as_mut() else {
                    return Task::none();
                };
                // A different runner process owns different terminals: every
                // id this editor holds names something that no longer exists.
                if mux
                    .incarnation
                    .is_some_and(|held| held != hello.incarnation)
                {
                    mux.terminals.clear();
                }
                mux.incarnation = Some(hello.incarnation);
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
                self.workspace.apply_snapshot(*workspace);
                self.reconcile_terminals()
            }
            MuxEvent::Workspace(snapshot) => {
                let Some(mux) = self.mux.as_mut() else {
                    return Task::none();
                };
                mux.workspace = Some((*snapshot).clone());
                self.workspace.apply_snapshot(*snapshot);
                self.reconcile_terminals()
            }
            MuxEvent::Reply(reply) => {
                let Some(mux) = self.mux.as_mut() else {
                    return Task::none();
                };
                mux.pending.remove(&reply.request);
                if let zeughaus_mux::CommandOutcome::Refused { reason } = reply.outcome {
                    self.hint = Some((reason, iced::time::Instant::now()));
                }
                Task::none()
            }
            MuxEvent::Lost => {
                let Some(mux) = self.mux.as_mut() else {
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
                // This address names a peer that is gone for good. The store
                // announces the replacement, and reconciliation dials it.
                self.mux = None;
                self.workspace.detach();
                Task::none()
            }
        }
    }

    /// Brings the terminal streams in line with what the workspace shows.
    ///
    /// One task per terminal any tab references; nothing for a terminal that
    /// was closed, and no second task for one already streaming. A terminal
    /// whose view survived a blink reattaches at the sequence it holds, so
    /// the runner continues with deltas instead of resending the screen.
    pub(super) fn reconcile_terminals(&mut self) -> Task<Message> {
        let epoch = self.mux_epoch;
        let Some(endpoint) = self.runtime.endpoint.clone() else {
            return Task::none();
        };
        let Some(mux) = self.mux.as_mut() else {
            return Task::none();
        };
        let wanted: Vec<zeughaus_mux::TerminalId> = mux
            .workspace
            .iter()
            .flat_map(|workspace| workspace.terminals())
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

        if epoch != self.mux_epoch {
            return Task::none();
        }
        let Some(mux) = self.mux.as_mut() else {
            return Task::none();
        };
        let Some(live) = mux.terminals.get_mut(&terminal) else {
            return Task::none();
        };
        match event {
            TerminalEvent::Ready(sender) => live.commands = Some(sender),
            TerminalEvent::Changed => {}
            // The task cleared the view and ended; attaching again asks for
            // a fresh head.
            TerminalEvent::Desynced => {
                live.commands = None;
                live.task = None;
                return self.reconcile_terminals();
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
    ) {
        if epoch != self.mux_epoch {
            return;
        }
        let page = match page {
            Ok(page) => page,
            Err(e) => {
                eprintln!("[mux] {terminal}: {e}");
                return;
            }
        };
        if let Some(live) = self
            .mux
            .as_mut()
            .and_then(|mux| mux.terminals.get_mut(&terminal))
            && let Some(view) = live.view.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            view.apply_page(page);
        }
    }

    /// What a terminal pane reported the user did.
    pub(super) fn apply_terminal_action(
        &mut self,
        pane: zeughaus_mux::PaneId,
        action: TerminalAction,
    ) -> Task<Message> {
        let Some(Surface::Terminal(terminal)) = self.workspace.surface_of(pane) else {
            return Task::none();
        };
        match action {
            TerminalAction::Command(command) => {
                self.send_input(terminal, command);
                Task::none()
            }
            TerminalAction::ScrollBy(lines) => self.scroll_terminal(terminal, lines),
            TerminalAction::Copy(text) => iced::clipboard::write(text),
            TerminalAction::OpenLink(url) => {
                // Opening is an explicit, separate action and not this cut's:
                // a terminal-supplied target must never reach a launcher by
                // way of a click the user did not mean as one.
                eprintln!("[mux] link: {url}");
                self.hint = Some((format!("link: {url}"), iced::time::Instant::now()));
                Task::none()
            }
            TerminalAction::TakeControl => {
                self.send_topology(zeughaus_mux::TopologyCommand::TakeControl { terminal });
                Task::none()
            }
            TerminalAction::Focused(focused) => {
                if focused {
                    self.workspace.focus(pane);
                }
                Task::none()
            }
        }
    }

    /// Sends one input command, remembering the serial it went out with.
    pub(super) fn send_input(
        &mut self,
        terminal: zeughaus_mux::TerminalId,
        command: zeughaus_mux::TerminalCommand,
    ) {
        let Some(live) = self
            .mux
            .as_mut()
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
        terminal: zeughaus_mux::TerminalId,
        lines: i64,
    ) -> Task<Message> {
        let epoch = self.mux_epoch;
        let Some(endpoint) = self.runtime.endpoint.clone() else {
            return Task::none();
        };
        let Some(live) = self
            .mux
            .as_mut()
            .and_then(|mux| mux.terminals.get_mut(&terminal))
        else {
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
        Task::batch(tasks)
    }

    /// Sends a structural change to the runner, or says why it cannot.
    pub(super) fn send_topology(&mut self, command: zeughaus_mux::TopologyCommand) {
        let refusal = match self.mux.as_mut() {
            None => Some("no runner: the shared workspace cannot be changed"),
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
        terminal: zeughaus_mux::TerminalId,
    ) -> Option<(&mux::SharedView, mux::Serials)> {
        let live = self.mux.as_ref()?.terminals.get(&terminal)?;
        let present = live
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        present.then_some((&live.view, live.serials))
    }

    /// What the pane's title bar calls a surface: a terminal's own title
    /// when one has arrived, the surface's word otherwise.
    pub(super) fn pane_title(&self, surface: Surface) -> String {
        match surface {
            Surface::Terminal(terminal) => self
                .terminal_view(terminal)
                .and_then(|(view, _)| {
                    let guard = view.lock().unwrap_or_else(|e| e.into_inner());
                    guard
                        .as_ref()
                        .map(|view| view.title.clone())
                        .filter(|title| !title.is_empty())
                })
                .unwrap_or_else(|| surface_title(surface).to_owned()),
            other => surface_title(other).to_owned(),
        }
    }

    /// One terminal pane.
    ///
    /// The terminal itself: one widget drawing the view, with the keyboard
    /// when its pane is focused and the runner's lease when this client
    /// holds it -- or when nobody does: the first client that types acquires
    /// an unclaimed terminal, so its keys must go out. A viewer sees the same
    /// rows and gets the take-control shortcut instead of the keys.
    pub(super) fn terminal_pane(
        &self,
        pane: Option<zeughaus_mux::PaneId>,
        terminal: zeughaus_mux::TerminalId,
    ) -> Element<'_, Message> {
        let Some((view, serials)) = self.terminal_view(terminal) else {
            return unavailable("Terminal", "Waiting for the runner's first screen.");
        };
        let controlling = {
            let guard = view.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().is_none_or(|view| {
                view.controller.as_ref().is_none_or(|controller| {
                    controller.client == mux::client_instance()
                        || self.mux.as_ref().and_then(|mux| mux.principal.as_deref())
                            == Some(controller.principal.as_str())
                })
            })
        };
        let focused = pane.is_some() && pane == self.workspace.focused_pane();
        let widget = iced_terminal::Terminal::new(Arc::clone(view), terminal.0)
            .controlling(controlling)
            .focused(focused)
            .next_serial(serials.next())
            .font_size(TERMINAL_FONT_SIZE);
        match pane {
            None => widget.into(),
            Some(pane) => widget
                .on_action(move |action| Message::TerminalAction(pane, action))
                .into(),
        }
    }
}
