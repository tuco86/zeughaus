//! The editor's client for the runner's terminal multiplexer: the exchanges
//! that carry the shared workspace and every terminal's screen.
//!
//! Three kinds of task, all on the one [`MUX_PATH`] so weida pools them onto
//! one QUIC connection -- the pool key includes the path, so a second one
//! would cost a second handshake and a warm attach would stop being warm:
//!
//! - [`control`], one per runner endpoint and long-lived. It attaches, hands
//!   the app a sender for [`TopologyCommand`]s, and reports the workspace,
//!   command replies and the connection's state.
//! - [`terminal`], one per attached terminal and long-lived. Input goes up,
//!   deltas and heads come down, and the two directions never queue behind
//!   each other.
//! - [`fetch`], one short exchange per scrollback page, so a fetch cannot
//!   head-of-line block either of the others.
//!
//! Nothing here redials. The first dial is [`first_dial`]'s and every later
//! one is weida's; what a redial does not restore is the application state,
//! so a `Lost` is reported upwards and the next `Connected` opens a fresh
//! exchange and attaches again with what this side still holds. A `GaveUp`
//! ends the task: a runner that restarted has a fresh identity, the store
//! announces the replacement, and the app starts a task for it.
//!
//! Native only, for the same reason as [`crate::feed`]: the wasm editor has
//! no sync layer, so it never learns where a runner serves.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};
use tokio::io::AsyncReadExt;
use weida::{IncomingTransfer, OutgoingTransfer, PeerEvent, PeerEvents, Requester, TransferMeta};
use zeughaus_mux::message::{Command, ControlAttached};
use zeughaus_mux::view::TerminalView;
use zeughaus_mux::{
    ClientHello, ClientInstanceId, CommandReply, FrameHeader, MAJOR, MINOR, Message as Wire,
    RowFetch, RowPage, RunnerIncarnation, ServerHello, TerminalAttach, TerminalCommand,
    TerminalHead, WireError, WorkspaceSnapshot,
};
use zeughaus_samples::MUX_PATH;

use crate::transport::{Endpoint, QUIC, client_tls, explain, first_dial, policy};

/// This editor process, as the runner names it.
///
/// One id for the whole process and for every redial: a control lease is
/// bound to it, so a network blink must not turn this window into a viewer of
/// the terminal it was typing into.
static CLIENT: LazyLock<ClientInstanceId> = LazyLock::new(|| {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No OS entropy is not a reason to fail to open a terminal. Two
        // `RandomState`s are seeded from the process's own random keys, which
        // is weaker than a CSPRNG and still distinct per process -- and this
        // id names a client, it does not authorize one.
        use std::hash::{BuildHasher, Hasher, RandomState};
        bytes[..8].copy_from_slice(&RandomState::new().build_hasher().finish().to_le_bytes());
        bytes[8..].copy_from_slice(&RandomState::new().build_hasher().finish().to_le_bytes());
    }
    ClientInstanceId::from_bytes(bytes)
});

/// The id this editor attaches with.
pub fn client_instance() -> ClientInstanceId {
    *CLIENT
}

/// Sends topology commands on the control exchange.
pub type CommandSender = mpsc::Sender<Command>;

/// Sends input on one terminal's exchange.
pub type TerminalSender = mpsc::Sender<TerminalCommand>;

/// What the control task reports.
#[derive(Debug, Clone)]
pub enum MuxEvent {
    /// The first event: how to send commands. Sent once, before any attach,
    /// so a command made while the first attach is still in flight is queued
    /// rather than lost.
    Ready(CommandSender),
    /// The runner answered an attach: hello, the whole topology and a head
    /// per terminal, which is everything a first paint needs.
    Attached {
        hello: ServerHello,
        workspace: Box<WorkspaceSnapshot>,
        heads: Vec<TerminalHead>,
    },
    /// The topology changed.
    Workspace(Box<WorkspaceSnapshot>),
    Reply(CommandReply),
    /// The exchange ended or the connection dropped. What is on screen is
    /// last-known until the next [`MuxEvent::Attached`].
    Lost,
    /// Weida stopped redialling: this endpoint names a peer that is gone.
    GaveUp,
}

/// The terminal state a pane shows, written by its task and read by the
/// widget: the task applies heads and deltas under a short lock and the
/// Elm loop only learns that something changed, never what.
pub type SharedView = iced_terminal::SharedView;

/// What one terminal's task reports. No payload beyond the first event: the
/// rows go straight into the [`SharedView`], and `Changed` is a wake.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// The first event: how to send input to this terminal.
    Ready(TerminalSender),
    /// The shared view moved; redraw.
    Changed,
    /// A delta did not fit the view's sequence. The view was cleared and
    /// the stream ended; attaching again asks for a fresh head, which is
    /// the only correct resync.
    Desynced,
    Error(WireError),
    /// The exchange ended. The app decides whether to attach again.
    Ended,
}

/// Puts a fresh head into a shared view, creating the view if this is the
/// first one. `apply_head` keeps the scroll position when the rows it names
/// still exist, which a rebuild from scratch would lose.
pub fn apply_head(view: &SharedView, head: zeughaus_mux::TerminalHead) {
    let mut guard = view.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(view) => view.apply_head(head),
        None => *guard = Some(TerminalView::from_head(head, ROW_CAPACITY)),
    }
}

/// How much of a terminal's scrollback one client keeps in rows. Ten screens
/// of a large terminal: enough that scrolling back a page is instant, bounded
/// so a hundred terminals cannot be a hundred unbounded caches.
pub const ROW_CAPACITY: usize = 4096;

/// The grid a terminal is attached at before anything has been laid out.
/// Replaced by the widget's measured geometry as soon as it draws.
pub const DEFAULT_GRID: zeughaus_mux::Dimensions = zeughaus_mux::Dimensions { cols: 80, rows: 24 };

/// What a terminal widget asks the app to do.
///
/// The widget holds `&TerminalView` and therefore cannot change anything: a
/// scroll is a request, not a mutation, and the app is the one side that can
/// both move the view and tell the runner which rows it now watches.
pub use iced_terminal::Action as TerminalAction;

/// The input serials one terminal's commands carry.
///
/// The runner echoes the highest it has applied, which is how the renderer
/// knows whether the cursor it holds is older than the last keystroke. Only
/// the highest matters, so a command whose serial arrives out of order cannot
/// rewind the counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Serials {
    last: u64,
}

impl Serials {
    /// The serial the next command must carry.
    pub fn next(&self) -> u64 {
        self.last.saturating_add(1)
    }

    /// Remembers the serial a command actually went out with.
    pub fn record(&mut self, serial: u64) {
        self.last = self.last.max(serial);
    }
}

/// How long an exchange has to last to count as a working one.
///
/// Below it the runner accepted and then ended the exchange -- it is shutting
/// down, it refused the attach, it is a standby -- and opening another at once
/// is a full-speed loop on a live connection.
const STABLE_UPTIME: Duration = Duration::from_secs(5);

/// Attaches to a runner's mux and keeps the attachment for as long as the
/// editor wants it.
///
/// The stream never ends on its own except on `GaveUp`: an ended exchange is
/// a reason to attach again, and the editor stops wanting this by dropping
/// the task.
pub fn control(endpoint: Endpoint) -> impl Stream<Item = MuxEvent> {
    // Room for a burst of snapshots and replies while the UI is mid-redraw.
    // A workspace snapshot is a few hundred bytes; the terminal rows that are
    // actually large travel on their own exchanges.
    iced::stream::channel(64, async move |mut out| {
        let Some(quic) = QUIC.as_ref() else {
            eprintln!("[mux] no QUIC runtime");
            return;
        };
        let url = match endpoint.path(MUX_PATH) {
            Ok(url) => url,
            Err(e) => {
                eprintln!("[mux] {e}");
                return;
            }
        };
        let requester = quic.requester(client_tls());
        // Watched from before the dial, so no transition is missed while the
        // first attach is in flight.
        let mut link = requester.events();
        if let Err(e) = first_dial(&url, || requester.connect(&url)).await {
            eprintln!("[mux] {e}");
            return;
        }

        let (sender, mut commands) = mpsc::channel(64);
        if out.send(MuxEvent::Ready(sender)).await.is_err() {
            return;
        }

        let mut known: Option<(RunnerIncarnation, u64)> = None;
        let mut attempt = 0u32;
        loop {
            if attempt > 0 {
                tokio::time::sleep(policy().delay(attempt)).await;
            }
            let started = Instant::now();
            let flow = attach(&requester, &mut known, &mut commands, &mut out, &mut link).await;
            attempt = next_attempt(attempt, started.elapsed());
            match flow {
                Flow::Retry => {
                    if out.send(MuxEvent::Lost).await.is_err() {
                        return;
                    }
                }
                Flow::Stop { gave_up } => {
                    if gave_up {
                        let _ = out.send(MuxEvent::GaveUp).await;
                    }
                    return;
                }
            }
        }
    })
}

/// Whether the control task opens another exchange.
enum Flow {
    Retry,
    Stop { gave_up: bool },
}

/// The attempt number the next exchange carries, given how long the one that
/// just ended lasted. Never zero, so an exchange that ended -- however well it
/// had been going -- is not reopened in the same instant.
fn next_attempt(attempt: u32, lasted: Duration) -> u32 {
    if lasted >= STABLE_UPTIME {
        1
    } else {
        attempt.saturating_add(1).min(16)
    }
}

/// A spawned half of an exchange, stopped when its owner goes away.
///
/// A `JoinHandle` dropped without `abort` leaves the task running: it would
/// hold the QUIC stream and its half of the exchange open long after the
/// editor dropped the endpoint. Iced aborts the outer task by dropping its
/// future, so the guard rather than a tidy exit path is what has to stop
/// these.
struct Abort(tokio::task::JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One control exchange, from the attach to whatever ends it.
///
/// The reply half is read by a spawned task rather than by a `select!`
/// branch: a frame is a header and then a body, and a `select!` that cancels
/// between the two would leave the stream pointing into the middle of a
/// message. The command half stays here, because the receiver has to outlive
/// the exchange.
async fn attach(
    requester: &Requester,
    known: &mut Option<(RunnerIncarnation, u64)>,
    commands: &mut mpsc::Receiver<Command>,
    out: &mut mpsc::Sender<MuxEvent>,
    link: &mut PeerEvents,
) -> Flow {
    let (mut request, reply) = match requester.open(TransferMeta::default()).await {
        Ok(halves) => halves,
        Err(e) => {
            eprintln!("[mux] open: {e}");
            return Flow::Retry;
        }
    };
    let hello = ClientHello {
        major: MAJOR,
        minor: MINOR,
        client: client_instance(),
        capabilities: Vec::new(),
        known_incarnation: known.map(|(incarnation, _)| incarnation),
        known_revision: known.map(|(_, revision)| revision),
    };
    let attach = Wire::ControlAttach(zeughaus_mux::ControlAttach { hello });
    if let Err(e) = write_frame(&mut request, &attach, 0).await {
        eprintln!("[mux] attach: {e}");
        return Flow::Retry;
    }
    let mut body = match reply.recv().await {
        Ok(body) => body,
        Err(e) => {
            eprintln!("[mux] attach: {e}");
            return Flow::Retry;
        }
    };
    let attached = match read_frame(&mut body).await {
        Ok(Some(Wire::ControlAttached(attached))) => attached,
        Ok(Some(Wire::Error(error))) => {
            eprintln!("[mux] attach refused: {}", error.message);
            return Flow::Retry;
        }
        Ok(Some(other)) => {
            eprintln!("[mux] attach answered with {:?}", other.kind());
            return Flow::Retry;
        }
        Ok(None) => return Flow::Retry,
        Err(e) => {
            eprintln!("[mux] attach: {e}");
            return Flow::Retry;
        }
    };
    let ControlAttached {
        hello,
        workspace,
        heads,
    } = attached;
    let incarnation = hello.incarnation;
    let revision = Arc::new(AtomicU64::new(workspace.revision));
    *known = Some((incarnation, workspace.revision));
    let attached = MuxEvent::Attached {
        hello,
        workspace: Box::new(workspace),
        heads,
    };
    if out.send(attached).await.is_err() {
        return Flow::Stop { gave_up: false };
    }

    let mut reader = Abort(tokio::spawn(read_control(
        body,
        out.clone(),
        revision.clone(),
    )));
    let flow = loop {
        tokio::select! {
            command = commands.next() => {
                // The app dropped its sender: this endpoint is no longer
                // wanted, and the task with it.
                let Some(command) = command else {
                    break Flow::Stop { gave_up: false };
                };
                let request_id = command.request.0;
                if let Err(e) = write_frame(&mut request, &Wire::Command(command), request_id).await
                {
                    eprintln!("[mux] command: {e}");
                    break Flow::Retry;
                }
            }
            // The runner ended the reply half: the attachment is over, and
            // what is on screen is last-known until the next one.
            _ = &mut reader.0 => break Flow::Retry,
            event = link.recv() => match event {
                // The endpoint is gone, which for one this task owns means
                // the task is ending.
                None => break Flow::Stop { gave_up: false },
                Some(PeerEvent::Lost { cause, .. }) => {
                    eprintln!("[mux] runner lost: {cause}");
                    break Flow::Retry;
                }
                Some(PeerEvent::GaveUp { why, .. }) => {
                    eprintln!("[mux] {}", explain(&why));
                    break Flow::Stop { gave_up: true };
                }
                Some(_) => {}
            },
        }
    };
    // The reply half belongs to this exchange; whatever it had left to say
    // is about a state the next attach replaces. `Abort` would do this on
    // drop as well; doing it here is what makes the order obvious.
    reader.0.abort();
    *known = Some((incarnation, revision.load(Ordering::Relaxed)));
    flow
}

/// Reads the control exchange's reply half until it ends.
async fn read_control(
    mut body: IncomingTransfer,
    mut out: mpsc::Sender<MuxEvent>,
    revision: Arc<AtomicU64>,
) {
    loop {
        let message = match read_frame(&mut body).await {
            Ok(Some(message)) => message,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[mux] control: {e}");
                return;
            }
        };
        let event = match message {
            Wire::WorkspaceSnapshot(snapshot) => {
                revision.store(snapshot.revision, Ordering::Relaxed);
                MuxEvent::Workspace(Box::new(snapshot))
            }
            Wire::CommandReply(reply) => MuxEvent::Reply(reply),
            Wire::Error(error) => {
                eprintln!("[mux] control: {}", error.message);
                continue;
            }
            // A kind that does not belong on this exchange is the runner's
            // bug, not a reason to tear the attachment down.
            other => {
                eprintln!("[mux] control: unexpected {:?}", other.kind());
                continue;
            }
        };
        if out.send(event).await.is_err() {
            return;
        }
    }
}

/// Streams one terminal: input up, deltas and heads down, applied into
/// `view` as they arrive.
///
/// Ends when the exchange does. The app owns whether a terminal is attached
/// at all -- it starts one of these per terminal the workspace references --
/// so a stream that ended is reported and not reopened from here.
pub fn terminal(
    endpoint: Endpoint,
    attach: TerminalAttach,
    view: SharedView,
) -> impl Stream<Item = TerminalEvent> {
    // Deltas are coalesced by the runner, so this only has to hold the few
    // that a mid-redraw moment can produce.
    iced::stream::channel(32, async move |mut out| {
        let Some(quic) = QUIC.as_ref() else {
            return;
        };
        let url = match endpoint.path(MUX_PATH) {
            Ok(url) => url,
            Err(e) => {
                eprintln!("[mux] {e}");
                return;
            }
        };
        let requester = quic.requester(client_tls());
        if let Err(e) = first_dial(&url, || requester.connect(&url)).await {
            eprintln!("[mux] {e}");
            return;
        }
        let terminal = attach.terminal;
        let (mut request, reply) = match requester.open(TransferMeta::default()).await {
            Ok(halves) => halves,
            Err(e) => {
                eprintln!("[mux] {terminal}: open: {e}");
                let _ = out.send(TerminalEvent::Ended).await;
                return;
            }
        };
        if let Err(e) = write_frame(&mut request, &Wire::TerminalAttach(attach), 0).await {
            eprintln!("[mux] {terminal}: attach: {e}");
            let _ = out.send(TerminalEvent::Ended).await;
            return;
        }
        let mut body = match reply.recv().await {
            Ok(body) => body,
            Err(e) => {
                eprintln!("[mux] {terminal}: attach: {e}");
                let _ = out.send(TerminalEvent::Ended).await;
                return;
            }
        };

        let (sender, mut input) = mpsc::channel::<TerminalCommand>(64);
        if out.send(TerminalEvent::Ready(sender)).await.is_err() {
            return;
        }
        // Input is written by its own task so a large delta being read cannot
        // hold a keystroke back: the two QUIC directions are independent, and
        // this is what keeps the code that way too.
        let writer = Abort(tokio::spawn(async move {
            while let Some(command) = input.next().await {
                if let Err(e) = write_frame(&mut request, &Wire::TerminalCommand(command), 0).await
                {
                    eprintln!("[mux] {terminal}: input: {e}");
                    return;
                }
            }
        }));

        loop {
            let message = match read_frame(&mut body).await {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[mux] {terminal}: {e}");
                    break;
                }
            };
            match message {
                Wire::TerminalAttached(attached) => {
                    if let Some(head) = attached.head {
                        apply_head(&view, head);
                    }
                }
                Wire::TerminalHead(head) => apply_head(&view, head),
                Wire::TerminalDelta(delta) => {
                    // The guard lives in this block and never across an
                    // await: a `MutexGuard` is not `Send`, and this stream
                    // runs on iced's executor.
                    let rejected = {
                        let mut guard = view.lock().unwrap_or_else(|e| e.into_inner());
                        // No head yet: the runner is about to send one, and
                        // a delta with nothing to apply it to is not an error.
                        let rejected = match guard.as_mut() {
                            Some(view) => view.apply_delta(delta).err(),
                            None => None,
                        };
                        if rejected.is_some() {
                            *guard = None;
                        }
                        rejected
                    };
                    if let Some(rejected) = rejected {
                        eprintln!("[mux] {terminal}: delta rejected ({rejected:?}), resyncing");
                        drop(writer);
                        let _ = out.send(TerminalEvent::Desynced).await;
                        return;
                    }
                }
                Wire::Error(error) => {
                    if out.send(TerminalEvent::Error(error)).await.is_err() {
                        break;
                    }
                    continue;
                }
                other => {
                    eprintln!("[mux] {terminal}: unexpected {:?}", other.kind());
                    continue;
                }
            }
            // A wake, not a queue: while one is pending the UI has not drawn
            // yet, and it will draw the newest state when it does.
            if let Err(e) = out.try_send(TerminalEvent::Changed)
                && e.is_disconnected()
            {
                break;
            }
        }
        drop(writer);
        let _ = out.send(TerminalEvent::Ended).await;
    })
}

/// Asks for scrollback rows the client scrolled to and does not hold.
///
/// Its own short exchange: a page is up to
/// [`zeughaus_mux::message::MAX_FETCH_ROWS`]
/// rows, and waiting for it on the control or terminal stream would stall
/// everything behind it.
pub async fn fetch(endpoint: Endpoint, wanted: RowFetch) -> Result<RowPage, String> {
    let quic = QUIC.as_ref().ok_or("no QUIC runtime")?;
    let url = endpoint.path(MUX_PATH)?;
    let requester = quic.requester(client_tls());
    first_dial(&url, || requester.connect(&url)).await?;
    let generation = wanted.generation;
    let (mut request, reply) = requester
        .open(TransferMeta::default())
        .await
        .map_err(|e| format!("fetch: open: {e}"))?;
    write_frame(&mut request, &Wire::RowFetch(wanted), generation).await?;
    // One question, so the request half is complete: the runner may answer as
    // soon as it has read it.
    request.finish().map_err(|e| format!("fetch: {e}"))?;
    let mut body = reply.recv().await.map_err(|e| format!("fetch: {e}"))?;
    match read_frame(&mut body).await? {
        Some(Wire::RowPage(page)) => Ok(page),
        Some(Wire::Error(error)) => Err(format!("fetch: {}", error.message)),
        Some(other) => Err(format!("fetch: answered with {:?}", other.kind())),
        None => Err("fetch: no answer".to_owned()),
    }
}

/// Writes one frame. The message is encoded whole first, so a frame is either
/// written completely or not at all -- a half-written body would desynchronize
/// the stream for good.
async fn write_frame(
    out: &mut OutgoingTransfer,
    message: &Wire,
    request_id: u64,
) -> Result<(), String> {
    let frame = message
        .encode(request_id)
        .map_err(|e| format!("{:?}: {e}", message.kind()))?;
    out.write_all(&frame)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// Reads one frame, or `None` when the stream ended cleanly.
///
/// The header is validated before the body is read, and
/// [`FrameHeader::decode`] refuses a length above the kind's bound -- so the
/// allocation below is bounded by the protocol and not by what a peer claims.
async fn read_frame(body: &mut IncomingTransfer) -> Result<Option<Wire>, String> {
    let mut header = [0u8; FrameHeader::LEN];
    // A stream that ended arrives here as a short read, not as an error.
    if body.read_exact(&mut header).await.is_err() {
        return Ok(None);
    }
    let header = FrameHeader::decode(&header).map_err(|e| format!("frame: {e}"))?;
    let mut payload = vec![0u8; header.length as usize];
    body.read_exact(&mut payload)
        .await
        .map_err(|e| format!("{:?} body: {e}", header.kind))?;
    let message =
        Wire::decode(header.kind, &payload).map_err(|e| format!("{:?}: {e}", header.kind))?;
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_mux::input::Key;
    use zeughaus_mux::{KeyInput, Modifiers};

    #[test]
    fn the_client_id_is_one_per_process() {
        assert_eq!(client_instance(), client_instance());
        assert_ne!(client_instance().0, [0u8; 16]);
    }

    #[test]
    fn a_serial_is_claimed_once_and_never_rewinds() {
        let mut serials = Serials::default();
        assert_eq!(serials.next(), 1);
        // Claiming is not using: the widget asks, the app records what went
        // out, and an unsent serial must not be skipped.
        assert_eq!(serials.next(), 1);

        serials.record(1);
        assert_eq!(serials.next(), 2);

        // A command whose reply crossed another's cannot pull the counter
        // back and make two commands share a serial.
        serials.record(5);
        serials.record(3);
        assert_eq!(serials.next(), 6);
    }

    #[test]
    fn an_input_command_carries_the_serial_it_was_given() {
        let mut serials = Serials::default();
        let command = TerminalCommand::Key {
            serial: serials.next(),
            input: KeyInput {
                key: Key::Char('a'),
                modifiers: Modifiers::default(),
            },
        };
        assert_eq!(command.serial(), Some(1));
        serials.record(command.serial().expect("a key carries a serial"));
        assert_eq!(serials.next(), 2);

        // A resize is not input and must not consume one.
        let resize = TerminalCommand::Resize(DEFAULT_GRID);
        assert_eq!(resize.serial(), None);
        assert_eq!(serials.next(), 2);
    }

    #[test]
    fn a_short_exchange_is_paced_but_a_long_one_is_not() {
        // An exchange the runner ends at once backs off further each time.
        assert_eq!(next_attempt(0, Duration::from_millis(5)), 1);
        assert_eq!(next_attempt(1, Duration::from_millis(5)), 2);
        // One that served for a while is reopened after the shortest wait
        // there is, not after the last drop's backoff.
        assert_eq!(next_attempt(9, STABLE_UPTIME), 1);
        // And the backoff is bounded, so the delay cannot overflow.
        assert_eq!(next_attempt(u32::MAX, Duration::ZERO), 16);
    }
}
