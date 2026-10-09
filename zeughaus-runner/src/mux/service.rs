//! The mux service: the workspace's authority, the terminal sessions, the
//! control leases, and the exchanges that serve them on `/mux`.
//!
//! Three exchange kinds arrive on the one replier, told apart by their first
//! frame (see `zeughaus_mux::message`): a control exchange per client, a
//! terminal exchange per (client, terminal), and short row fetches. Every
//! exchange runs its request and reply directions concurrently, so a
//! client's keystroke never waits behind a large delta and a large delta
//! never waits behind a keystroke.
//!
//! The workspace lives in one mutex and every structural change goes through
//! it, so two clients splitting the same pane at once are serialized and
//! both get a consistent revision. Terminal output is never in that mutex:
//! each session has its own model lock, and a subscriber computes its delta
//! from the sequence number it last sent -- a slow client gets fewer,
//! larger deltas, never a queue of them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_mux::message::{Capability, Command, ErrorCode};
use zeughaus_mux::workspace::ProfileId;
use zeughaus_mux::{
    ClientHello, ClientInstanceId, CommandOutcome, CommandReply, ControlAttach, Controller,
    Dimensions, MAJOR, MINOR, Message, RequestId, RowFetch, RowPage, RunnerIncarnation,
    ServerHello, TerminalAttach, TerminalAttached, TerminalCommand, TerminalId, TopologyCommand,
    WireError, WorkspaceSnapshot,
};
use zeughaus_terminal::{Profile, Session, TerminalHost};

use super::frames::{ReadError, read_frame, write_frame};
use super::persist::{self, SavedRun, SavedTerminal};
use super::workspace::Workspace;
use super::{GraphSync, OwnedPlacement};
use crate::transport::principal_of;

/// Scrollback rows sent with a head, above the visible ones: enough that a
/// wheel notch or two needs no fetch, bounded so an attach stays small.
const HEAD_ROWS_ABOVE: usize = 128;

/// How long after a client vanishes its leases are held for it, so a network
/// blink does not make its terminals read-only.
const LEASE_GRACE: Duration = Duration::from_secs(10);

/// The render cadence a subscriber coalesces at during a burst: a delta at
/// most this often per terminal per client, whatever the child prints. A
/// change after quiet goes out at once -- a keystroke's echo is not a burst,
/// and waiting for company would put this whole window on every keystroke.
const COALESCE: Duration = Duration::from_millis(12);

/// Replies remembered per client for deduplicating a command resent after a
/// redial.
const REMEMBERED_REPLIES: usize = 64;

/// Most clients attached at once. Beyond it an attach is refused.
const MAX_CLIENTS: usize = 64;

/// Who controls a terminal, and until when that is still theirs after they
/// went away.
struct Lease {
    client: ClientInstanceId,
    principal: String,
    /// Set when the client's control exchange ended; `None` while it is
    /// attached.
    detached_at: Option<Instant>,
}

impl Lease {
    fn controller(&self) -> Controller {
        Controller {
            client: self.client,
            principal: self.principal.clone(),
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.detached_at
            .is_some_and(|at| now.duration_since(at) >= LEASE_GRACE)
    }
}

struct ClientState {
    principal: String,
    /// Replies already given, by request id: a command resent after a
    /// redial is answered from here, not applied twice.
    replies: BTreeMap<RequestId, CommandOutcome>,
}

/// What a restarted runner needs to reattach a terminal besides its id,
/// and what it needs to finish a job run that outlived its starter.
struct TerminalMeta {
    label: String,
    scrollback_rows: usize,
    run: Option<SavedRun>,
}

/// The document's graphs as far as the panes that show them care.
#[derive(Default)]
struct Graphs {
    /// This runner's top-level graphs that already had their chance at a
    /// tab in this process. Not saved: after a restart every graph of this
    /// runner's that no pane shows gets a tab again.
    seen: BTreeSet<u64>,
    /// Every node's display name, for tabs named after a graph.
    names: HashMap<u64, String>,
    /// Every node id in the document; `None` until the first sync.
    exists: Option<HashSet<u64>>,
}

struct Inner {
    incarnation: RunnerIncarnation,
    profiles: Vec<Profile>,
    host: TerminalHost,
    workspace: Mutex<Workspace>,
    sessions: Mutex<HashMap<TerminalId, Session>>,
    /// Taken after `sessions` and `workspace` when all three are needed.
    meta: Mutex<HashMap<TerminalId, TerminalMeta>>,
    /// Taken after `workspace`.
    graphs: Mutex<Graphs>,
    leases: Mutex<HashMap<TerminalId, Lease>>,
    clients: Mutex<HashMap<ClientInstanceId, ClientState>>,
    next_terminal: AtomicU64,
    /// The workspace revision, for control exchanges to wake on.
    revision: watch::Sender<u64>,
}

impl Inner {
    fn publish_revision(&self, revision: u64) {
        // PTY readers and topology commands publish concurrently, after
        // releasing the workspace lock. A late publisher must not rewind it.
        self.revision.send_if_modified(|current| {
            if revision > *current {
                *current = revision;
                true
            } else {
                false
            }
        });
    }
}

/// The service handle. Cheap to clone; one per runner.
#[derive(Clone)]
pub struct MuxService {
    inner: Arc<Inner>,
}

impl MuxService {
    /// The service over `host`. With shims, the terminals a previous runner
    /// left in them are reattached and the workspace that showed them is
    /// restored around the ones that came back.
    pub fn new(
        incarnation: RunnerIncarnation,
        profiles: Vec<Profile>,
        host: TerminalHost,
    ) -> MuxService {
        let profiles = if profiles.is_empty() {
            vec![Profile::default_shell()]
        } else {
            profiles
        };
        let (revision, _) = watch::channel(1);
        let service = MuxService {
            inner: Arc::new(Inner {
                incarnation,
                profiles,
                host,
                workspace: Mutex::new(Workspace::new(incarnation)),
                sessions: Mutex::new(HashMap::new()),
                meta: Mutex::new(HashMap::new()),
                graphs: Mutex::new(Graphs::default()),
                leases: Mutex::new(HashMap::new()),
                clients: Mutex::new(HashMap::new()),
                next_terminal: AtomicU64::new(1),
                revision,
            }),
        };
        #[cfg(unix)]
        if let TerminalHost::Shim(shim) = &service.inner.host {
            service.restore(shim);
        }
        service
    }

    /// Where the workspace is saved: beside the shims' root. `None` when
    /// terminals die with this process and there is nothing to restore.
    fn state_file(&self) -> Option<std::path::PathBuf> {
        match &self.inner.host {
            TerminalHost::Local => None,
            #[cfg(unix)]
            TerminalHost::Shim(shim) => Some(shim.root.with_file_name("workspace.json")),
        }
    }

    #[cfg(unix)]
    fn restore(&self, shim: &zeughaus_terminal::ShimHost) {
        let saved = self.state_file().and_then(|path| persist::load(&path));
        let mut restored: HashMap<TerminalId, Session> = HashMap::new();
        let mut meta: HashMap<TerminalId, TerminalMeta> = HashMap::new();
        for terminal in saved.iter().flat_map(|saved| &saved.terminals) {
            match Session::reattach(
                terminal.id,
                shim,
                &terminal.label,
                terminal.scrollback_rows,
                terminal.size,
            ) {
                Ok(session) => {
                    self.follow_title(&session);
                    restored.insert(terminal.id, session);
                    meta.insert(
                        terminal.id,
                        TerminalMeta {
                            label: terminal.label.clone(),
                            scrollback_rows: terminal.scrollback_rows,
                            run: terminal.run.clone(),
                        },
                    );
                }
                Err(e) => eprintln!("[mux] terminal {} not restored: {e}", terminal.id.0),
            }
        }
        let workspace = match &saved {
            Some(saved) => Workspace::restore(self.inner.incarnation, saved, |id| {
                restored.contains_key(&id)
            }),
            None => Workspace::new(self.inner.incarnation),
        };
        // A terminal no pane shows and the runner does not own would be a
        // shell nobody can ever reach again.
        restored.retain(|id, session| {
            let known = workspace.knows(*id);
            if !known {
                session.kill();
                meta.remove(id);
            }
            known
        });
        // Shims nothing refers to any more: a terminal that did not
        // reattach, or one whose workspace entry was lost with the file.
        let mut highest_dir = 0;
        if let Ok(entries) = std::fs::read_dir(&shim.root) {
            for entry in entries.flatten() {
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u64>().ok())
                else {
                    continue;
                };
                highest_dir = highest_dir.max(id);
                if !restored.contains_key(&TerminalId(id)) {
                    zeughaus_terminal::shim::close_dir(&entry.path());
                }
            }
        }
        let highest_restored = restored.keys().map(|id| id.0).max().unwrap_or(0);
        let next = saved
            .as_ref()
            .map_or(1, |saved| saved.next_terminal)
            .max(highest_dir + 1)
            .max(highest_restored + 1);
        self.inner.next_terminal.store(next, Ordering::Relaxed);
        if !restored.is_empty() {
            eprintln!("[mux] restored {} terminals", restored.len());
        }
        *self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = restored;
        *self
            .inner
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = workspace;
        *self.inner.meta.lock().unwrap_or_else(|e| e.into_inner()) = meta;
        self.persist();
    }

    /// Saves the workspace and its terminals for the next runner. Nothing
    /// to do when terminals end with this process.
    pub fn persist(&self) {
        let Some(path) = self.state_file() else {
            return;
        };
        let saved = {
            // The lock order `snapshot` takes, and `meta` last.
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let meta = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
            let mut terminals: Vec<SavedTerminal> = sessions
                .iter()
                .filter_map(|(id, session)| {
                    let meta = meta.get(id)?;
                    Some(SavedTerminal {
                        id: *id,
                        label: meta.label.clone(),
                        scrollback_rows: meta.scrollback_rows,
                        size: session.size(),
                        run: meta.run.clone(),
                    })
                })
                .collect();
            terminals.sort_by_key(|t| t.id);
            workspace.to_saved(self.inner.next_terminal.load(Ordering::Relaxed), terminals)
        };
        if let Err(e) = persist::store(&path, &saved) {
            eprintln!("[mux] cannot save {}: {e}", path.display());
        }
    }

    /// The job runs among the terminals this process reattached: runs a
    /// previous runner started and never saw end.
    pub fn restored_runs(&self) -> Vec<(TerminalId, SavedRun)> {
        let meta = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
        let mut runs: Vec<(TerminalId, SavedRun)> = meta
            .iter()
            .filter_map(|(id, meta)| Some((*id, meta.run.clone()?)))
            .collect();
        runs.sort_by_key(|(id, _)| *id);
        runs
    }

    /// Starts a terminal the runner owns: a job's process, not a pane's
    /// shell. `placement` says whether a pane shows it from the start; a
    /// detached one waits for a client to attach it. Closing a pane that
    /// shows it detaches it again instead of ending the child; only
    /// [`MuxService::close_terminal`], closing its tab in the locked group
    /// and the runner's exit do that.
    ///
    /// `log` is appended every byte the program writes, which is how a
    /// run's log is recorded; `run` is what a restarted runner needs to
    /// finish the run's record.
    pub fn spawn_owned(
        &self,
        profile: Profile,
        log: Option<std::path::PathBuf>,
        run: Option<SavedRun>,
        placement: OwnedPlacement,
    ) -> Result<TerminalId, String> {
        let id = TerminalId(self.inner.next_terminal.fetch_add(1, Ordering::Relaxed));
        let session = Session::spawn(
            id,
            &profile,
            Dimensions { cols: 80, rows: 24 },
            log.as_deref(),
            &self.inner.host,
        )
        .map_err(|e| format!("cannot start {}: {e}", profile.label))?;
        self.follow_title(&session);
        let revision = {
            // The lock order is the one `snapshot` takes: sessions, then
            // the workspace.
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            sessions.insert(id, session);
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            workspace.add_owned(id, placement);
            self.inner
                .meta
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id,
                    TerminalMeta {
                        label: profile.label,
                        scrollback_rows: profile.scrollback_rows,
                        run,
                    },
                );
            workspace.revision()
        };
        self.inner.publish_revision(revision);
        self.persist();
        Ok(id)
    }

    /// Brings the graph panes in line with the document, as
    /// [`Workspace::sync_graphs`] describes, and keeps the names tabs are
    /// titled with and the ids `OpenGraph` is checked against.
    pub fn sync_graphs(&self, update: GraphSync) {
        let revision = {
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut graphs = self.inner.graphs.lock().unwrap_or_else(|e| e.into_inner());
            let graphs = &mut *graphs;
            let changed = workspace.sync_graphs(&update, &graphs.names, &mut graphs.seen);
            graphs.names = update.names;
            graphs.exists = Some(update.exists);
            changed.then(|| workspace.revision())
        };
        if let Some(revision) = revision {
            self.inner.publish_revision(revision);
            self.persist();
        }
    }

    /// Ends a terminal the runner owns and forgets it, whether a pane shows
    /// it or not. The same path a client's `CloseTerminal` takes.
    pub fn close_terminal(&self, terminal: TerminalId) -> Result<(), String> {
        match self.structural(&TopologyCommand::CloseTerminal { terminal }) {
            CommandOutcome::Applied { .. } => Ok(()),
            CommandOutcome::Refused { reason } => Err(reason),
        }
    }

    /// Moves a terminal out of the locked group into the detached list, as
    /// [`Workspace::hide_owned`] describes.
    pub fn hide_terminal(&self, terminal: TerminalId) {
        let revision = {
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            workspace.hide_owned(terminal).then(|| workspace.revision())
        };
        if let Some(revision) = revision {
            self.inner.publish_revision(revision);
            self.persist();
        }
    }

    /// Accepts exchanges until the replier goes away, which for this process
    /// means never.
    pub async fn accept(self, replier: Replier) {
        loop {
            match replier.accept().await {
                Ok(request) => {
                    let service = self.clone();
                    tokio::spawn(async move { service.serve(request).await });
                }
                Err(e) => {
                    eprintln!("[mux] stopped accepting: {e}");
                    return;
                }
            }
        }
    }

    /// One exchange: the first frame says which kind.
    async fn serve(self, mut request: IncomingRequest) {
        let Some(principal) = principal_of(request.meta()) else {
            eprintln!("[mux] refused an exchange from a peer that proved no identity");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        };
        let mut body = request.take_body();
        let canceled = request.canceled();
        let first = match read_frame(&mut body).await {
            Ok((_, message)) => message,
            Err(e) => {
                eprintln!("[mux] {principal}: unreadable first frame: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[mux] {principal}: cannot open the reply half: {e}");
                return;
            }
        };
        let outcome = match first {
            Message::ControlAttach(attach) => {
                self.serve_control(principal, attach, body, &mut reply, canceled)
                    .await
            }
            Message::TerminalAttach(attach) => {
                self.serve_terminal(principal, attach, body, &mut reply, canceled)
                    .await
            }
            Message::RowFetch(fetch) => self.serve_fetch(fetch, &mut reply).await,
            other => Err(WireError {
                code: ErrorCode::Protocol,
                message: format!("{:?} cannot open an exchange", other.kind()),
            }),
        };
        if let Err(error) = outcome {
            // Best effort: the peer may already be gone, and then there is
            // nobody to tell.
            let _ = write_frame(&mut reply, &Message::Error(error), 0).await;
        }
        let _ = reply.finish();
    }

    async fn serve_control(
        &self,
        principal: String,
        attach: ControlAttach,
        mut body: weida::IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
        canceled: impl Future<Output = ()>,
    ) -> Result<(), WireError> {
        let hello = attach.hello;
        if hello.major != MAJOR {
            return Err(WireError {
                code: ErrorCode::Version,
                message: format!("protocol {} is not {MAJOR}", hello.major),
            });
        }
        let client = hello.client;
        self.register(client, &principal)?;
        let mut revision = self.inner.revision.subscribe();
        revision.borrow_and_update();
        let attached = self.attached(&hello, &principal);
        let mut sent_revision = attached.workspace.revision;
        write_frame(reply, &Message::ControlAttached(attached), 0)
            .await
            .map_err(|e| gone(&e))?;

        let mut canceled = std::pin::pin!(canceled);
        let result = loop {
            tokio::select! {
                () = &mut canceled => break Ok(()),
                changed = revision.changed() => {
                    if changed.is_err() {
                        break Ok(());
                    }
                    let current = *revision.borrow_and_update();
                    if current > sent_revision {
                        let snapshot = self.snapshot();
                        sent_revision = snapshot.revision;
                        if let Err(e) = write_frame(reply, &Message::WorkspaceSnapshot(snapshot), 0).await {
                            break Err(gone(&e));
                        }
                    }
                }
                frame = read_frame(&mut body) => {
                    let message = match frame {
                        Ok((_, message)) => message,
                        Err(ReadError::Ended) | Err(ReadError::Io(_)) => break Ok(()),
                        Err(ReadError::Codec(e)) => break Err(WireError {
                            code: ErrorCode::Protocol,
                            message: e.to_string(),
                        }),
                    };
                    let Message::Command(command) = message else {
                        break Err(WireError {
                            code: ErrorCode::Protocol,
                            message: format!("{:?} on a control exchange", message.kind()),
                        });
                    };
                    let outcome = self.command(client, &principal, &command);
                    let reply_message = Message::CommandReply(CommandReply {
                        request: command.request,
                        outcome,
                    });
                    if let Err(e) = write_frame(reply, &reply_message, command.request.0).await {
                        break Err(gone(&e));
                    }
                }
            }
        };
        self.detach(client);
        result
    }

    async fn serve_terminal(
        &self,
        principal: String,
        attach: TerminalAttach,
        mut body: weida::IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
        canceled: impl Future<Output = ()>,
    ) -> Result<(), WireError> {
        let client = attach.client;
        let terminal = attach.terminal;
        let session = self.session(terminal).ok_or(WireError {
            code: ErrorCode::UnknownTerminal,
            message: format!("no {terminal}"),
        })?;
        let mut changes = session.changes();
        let current = *changes.borrow_and_update();
        let epoch = session.epoch();
        let mut head = match attach.known {
            Some(known) if known == (epoch, current) => None,
            _ => Some(session.head(HEAD_ROWS_ABOVE)),
        };
        // Serials are per client and per exchange: a fresh attach starts
        // its count at zero, whatever the engine has applied for others.
        if let Some(head) = head.as_mut() {
            head.input_serial_ack = 0;
        }
        let mut acked_serial = 0u64;
        let mut sent_seq = head.as_ref().map_or(current, |h| h.seq);
        let mut sent_epoch = head.as_ref().map_or(epoch, |h| h.epoch);
        write_frame(
            reply,
            &Message::TerminalAttached(TerminalAttached { head }),
            0,
        )
        .await
        .map_err(|e| gone(&e))?;

        // Every row written since the last delta travels, wherever the
        // screen has scrolled it to, so what a client is scrolled to needs no
        // bookkeeping here; `Viewport` is accepted and changes nothing.
        let mut size = attach.size;
        if self.controls(terminal, client) {
            let _ = session.apply(&TerminalCommand::Resize(size));
        }

        let mut canceled = std::pin::pin!(canceled);
        let mut last_sent = Instant::now() - COALESCE;
        loop {
            tokio::select! {
                () = &mut canceled => break Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        break Ok(());
                    }
                    // Pace, do not delay: a change within the cadence of the
                    // last delta waits for the rest of it, a change after
                    // quiet is sent as it is.
                    let due = last_sent + COALESCE;
                    let now = Instant::now();
                    if due > now {
                        tokio::time::sleep(due - now).await;
                    }
                    changes.borrow_and_update();
                    last_sent = Instant::now();
                    let delta = session.delta_since(sent_seq);
                    // A screen switch renumbers the rows: what the client
                    // holds is of the other screen, and only a head replaces
                    // it.
                    let message = if delta.epoch == sent_epoch {
                        let mut delta = delta;
                        delta.input_serial_ack = acked_serial;
                        sent_seq = delta.to_seq;
                        Message::TerminalDelta(delta)
                    } else {
                        let mut head = session.head(HEAD_ROWS_ABOVE);
                        head.input_serial_ack = acked_serial;
                        sent_seq = head.seq;
                        sent_epoch = head.epoch;
                        Message::TerminalHead(head)
                    };
                    if let Err(e) = write_frame(reply, &message, 0).await {
                        break Err(gone(&e));
                    }
                    if !self.exists(terminal) {
                        // The pane was closed: the final delta carried the
                        // exit, and there is nothing more to say.
                        break Ok(());
                    }
                }
                frame = read_frame(&mut body) => {
                    let message = match frame {
                        Ok((_, message)) => message,
                        Err(ReadError::Ended) | Err(ReadError::Io(_)) => break Ok(()),
                        Err(ReadError::Codec(e)) => break Err(WireError {
                            code: ErrorCode::Protocol,
                            message: e.to_string(),
                        }),
                    };
                    let Message::TerminalCommand(command) = message else {
                        break Err(WireError {
                            code: ErrorCode::Protocol,
                            message: format!("{:?} on a terminal exchange", message.kind()),
                        });
                    };
                    match command {
                        TerminalCommand::Viewport { .. } => {}
                        TerminalCommand::Resize(d) => {
                            size = d;
                            if self.acquire_if_free(terminal, client, &principal, &session)
                                && let Err(e) = session.apply(&TerminalCommand::Resize(size))
                            {
                                eprintln!("[mux] {terminal}: resize refused: {e}");
                            }
                        }
                        input => {
                            // Acknowledged whether or not it is applied: a
                            // viewer's keystroke is handled by being dropped,
                            // and its cursor must not wait for it forever.
                            if let Some(serial) = input.serial() {
                                acked_serial = acked_serial.max(serial);
                            }
                            // The first client that types acquires an unowned
                            // terminal; a viewer's input is dropped, never
                            // queued -- taking control is an explicit command.
                            if self.acquire_if_free(terminal, client, &principal, &session)
                                && let Err(e) = session.apply(&input)
                            {
                                eprintln!("[mux] {terminal}: input refused: {e}");
                            }
                        }
                    }
                }
            }
        }
    }

    async fn serve_fetch(
        &self,
        fetch: RowFetch,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), WireError> {
        let session = self.session(fetch.terminal).ok_or(WireError {
            code: ErrorCode::UnknownTerminal,
            message: format!("no {}", fetch.terminal),
        })?;
        let Some((seq, first_retained, rows)) = session.rows(fetch.epoch, fetch.range) else {
            return Err(WireError {
                code: ErrorCode::Stale,
                message: "unknown epoch".to_owned(),
            });
        };
        let page = RowPage {
            terminal: fetch.terminal,
            epoch: fetch.epoch,
            generation: fetch.generation,
            first_retained,
            seq,
            rows,
        };
        write_frame(reply, &Message::RowPage(page), fetch.generation)
            .await
            .map_err(|e| gone(&e))
    }

    // --- state -------------------------------------------------------------

    fn register(&self, client: ClientInstanceId, principal: &str) -> Result<(), WireError> {
        let mut clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = clients.get(&client) {
            if state.principal != principal {
                // An instance id is not a credential: a different principal
                // presenting a known id is refused rather than inheriting its
                // leases and replies.
                return Err(WireError {
                    code: ErrorCode::Unauthorized,
                    message: "client instance belongs to another principal".to_owned(),
                });
            }
        } else {
            if clients.len() >= MAX_CLIENTS {
                return Err(WireError {
                    code: ErrorCode::Limit,
                    message: format!("{MAX_CLIENTS} clients attached already"),
                });
            }
            clients.insert(
                client,
                ClientState {
                    principal: principal.to_owned(),
                    replies: BTreeMap::new(),
                },
            );
        }
        drop(clients);
        // Back within the grace: the leases are theirs again.
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        for lease in leases.values_mut() {
            if lease.client == client {
                lease.detached_at = None;
            }
        }
        Ok(())
    }

    /// Marks the client's leases as detached and, once the grace has run
    /// out without it coming back, releases them for everyone to see: a
    /// terminal whose controller is gone for good must not keep a
    /// `controller` that stops every other editor from typing into it.
    fn detach(&self, client: ClientInstanceId) {
        let now = Instant::now();
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        for lease in leases.values_mut() {
            if lease.client == client && lease.detached_at.is_none() {
                lease.detached_at = Some(now);
            }
        }
        drop(leases);
        let service = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(LEASE_GRACE).await;
            service.release_expired(client);
        });
    }

    /// Drops every lease `client` still holds detached and past its grace.
    fn release_expired(&self, client: ClientInstanceId) {
        let now = Instant::now();
        let expired: Vec<TerminalId> = {
            let leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
            leases
                .iter()
                .filter(|(_, lease)| lease.client == client && lease.expired(now))
                .map(|(terminal, _)| *terminal)
                .collect()
        };
        for terminal in expired {
            let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
            if leases
                .get(&terminal)
                .is_some_and(|lease| lease.client == client && lease.expired(now))
            {
                leases.remove(&terminal);
                drop(leases);
                if let Some(session) = self.session(terminal) {
                    session.set_controller(None);
                }
            }
        }
    }

    fn attached(
        &self,
        hello: &ClientHello,
        principal: &str,
    ) -> zeughaus_mux::message::ControlAttached {
        let workspace = self.snapshot();
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let heads = workspace
            .terminals()
            .filter_map(|t| sessions.get(&t).map(|s| s.head(0)))
            .collect();
        let _ = hello;
        zeughaus_mux::message::ControlAttached {
            hello: ServerHello {
                major: MAJOR,
                // The lower of the two minors; with this side at zero that
                // is always zero, and a real negotiation comes with MINOR 1.
                minor: MINOR,
                incarnation: self.inner.incarnation,
                capabilities: Vec::<Capability>::new(),
                principal: principal.to_owned(),
                profiles: self
                    .inner
                    .profiles
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (i as u32, p.label.clone()))
                    .collect(),
            },
            workspace,
            heads,
        }
    }

    fn snapshot(&self) -> WorkspaceSnapshot {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = self
            .inner
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let graphs = self.inner.graphs.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = workspace.snapshot(&|t| sessions.get(&t).map(|s| s.title()), &|g| {
            graphs.names.get(&g).cloned()
        });
        // Heads may carry a controller the workspace does not know; the
        // snapshot is structure only. Nothing to merge.
        snapshot.incarnation = self.inner.incarnation;
        snapshot
    }

    /// Makes `session`'s title changes workspace revisions, so a tab named
    /// after the terminal is renamed for every client without any of them
    /// streaming the terminal. Registered before the session is published,
    /// so no title set after the snapshot that first shows it is lost.
    fn follow_title(&self, session: &Session) {
        // Weak: the service owns the session, which owns this listener.
        let inner = Arc::downgrade(&self.inner);
        let terminal = session.id();
        session.on_title_change(move || {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let revision = {
                let mut workspace = inner.workspace.lock().unwrap_or_else(|e| e.into_inner());
                if !workspace.title_changed(terminal) {
                    return;
                }
                workspace.revision()
            };
            inner.publish_revision(revision);
        });
    }

    /// The session behind a terminal id, if it still exists.
    pub fn session(&self, terminal: TerminalId) -> Option<Session> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&terminal)
            .cloned()
    }

    fn exists(&self, terminal: TerminalId) -> bool {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&terminal)
    }

    fn controls(&self, terminal: TerminalId, client: ClientInstanceId) -> bool {
        self.inner
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&terminal)
            .is_some_and(|lease| lease.client == client)
    }

    /// Whether `client` may drive `terminal`: it holds the lease, or nobody
    /// does, or the holder is gone -- for good, or briefly but under the same
    /// principal (the same user's restarted editor, which a grace meant for
    /// a network blink must not lock out of its own shell).
    fn acquire_if_free(
        &self,
        terminal: TerminalId,
        client: ClientInstanceId,
        principal: &str,
        session: &Session,
    ) -> bool {
        let now = Instant::now();
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        match leases.get(&terminal) {
            Some(lease) if lease.client == client => return true,
            Some(lease) if lease.detached_at.is_some() && lease.principal == principal => {}
            Some(lease) if !lease.expired(now) => return false,
            _ => {}
        }
        let lease = Lease {
            client,
            principal: principal.to_owned(),
            detached_at: None,
        };
        session.set_controller(Some(lease.controller()));
        leases.insert(terminal, lease);
        true
    }

    fn take_control(
        &self,
        terminal: TerminalId,
        client: ClientInstanceId,
        principal: &str,
    ) -> CommandOutcome {
        let Some(session) = self.session(terminal) else {
            return CommandOutcome::Refused {
                reason: format!("no {terminal}"),
            };
        };
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        let lease = Lease {
            client,
            principal: principal.to_owned(),
            detached_at: None,
        };
        session.set_controller(Some(lease.controller()));
        leases.insert(terminal, lease);
        CommandOutcome::Applied {
            revision: *self.inner.revision.borrow(),
        }
    }

    fn release_control(&self, terminal: TerminalId, client: ClientInstanceId) -> CommandOutcome {
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        if leases.get(&terminal).is_some_and(|l| l.client == client) {
            leases.remove(&terminal);
            if let Some(session) = self.session(terminal) {
                session.set_controller(None);
            }
        }
        CommandOutcome::Applied {
            revision: *self.inner.revision.borrow(),
        }
    }

    /// Applies a command once per (client, request id).
    fn command(
        &self,
        client: ClientInstanceId,
        principal: &str,
        command: &Command,
    ) -> CommandOutcome {
        {
            let clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(outcome) = clients
                .get(&client)
                .and_then(|c| c.replies.get(&command.request))
            {
                return outcome.clone();
            }
        }
        let outcome = match &command.command {
            TopologyCommand::TakeControl { terminal } => {
                self.take_control(*terminal, client, principal)
            }
            TopologyCommand::ReleaseControl { terminal } => self.release_control(*terminal, client),
            structural => self.structural(structural),
        };
        let mut clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = clients.get_mut(&client) {
            state.replies.insert(command.request, outcome.clone());
            while state.replies.len() > REMEMBERED_REPLIES {
                state.replies.pop_first();
            }
        }
        outcome
    }

    fn structural(&self, command: &TopologyCommand) -> CommandOutcome {
        let mut spawned: Vec<(TerminalId, Session, TerminalMeta)> = Vec::new();
        let mut spawn = |profile: ProfileId| -> Result<TerminalId, String> {
            let profile = self
                .inner
                .profiles
                .get(profile.0 as usize)
                .ok_or_else(|| format!("no profile {}", profile.0))?;
            let id = TerminalId(self.inner.next_terminal.fetch_add(1, Ordering::Relaxed));
            let session = Session::spawn(
                id,
                profile,
                Dimensions { cols: 80, rows: 24 },
                None,
                &self.inner.host,
            )
            .map_err(|e| format!("cannot start {}: {e}", profile.label))?;
            let meta = TerminalMeta {
                label: profile.label.clone(),
                scrollback_rows: profile.scrollback_rows,
                run: None,
            };
            spawned.push((id, session, meta));
            Ok(id)
        };
        let result = {
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.known_graph(command).and_then(|()| {
                workspace
                    .apply(command, &mut spawn)
                    .map(|applied| (workspace.revision(), applied))
            })
        };
        match result {
            Ok((revision, applied)) => {
                {
                    let mut sessions = self
                        .inner
                        .sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let mut metas = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
                    for (id, session, meta) in spawned {
                        self.follow_title(&session);
                        sessions.insert(id, session);
                        metas.insert(id, meta);
                    }
                    let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
                    for id in &applied.killed {
                        leases.remove(id);
                        metas.remove(id);
                        if let Some(session) = sessions.remove(id) {
                            session.kill();
                        }
                    }
                }
                self.inner.publish_revision(revision);
                self.persist();
                CommandOutcome::Applied { revision }
            }
            Err(reason) => {
                // A spawn that succeeded before the command failed is a
                // child nobody shows: end it.
                for (_, session, _) in spawned {
                    session.kill();
                }
                CommandOutcome::Refused { reason }
            }
        }
    }

    /// Refuses to open a graph the document does not hold, which would be a
    /// pane with nothing to show. Before the first sync nothing is known and
    /// everything is let through; a sync removes the pane if the graph turns
    /// out to be gone. Called with the workspace locked, as the lock order
    /// asks.
    fn known_graph(&self, command: &TopologyCommand) -> Result<(), String> {
        if let TopologyCommand::OpenGraph { graph } = command
            && self
                .inner
                .graphs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .exists
                .as_ref()
                .is_some_and(|ids| !ids.contains(graph))
        {
            return Err(format!("no graph {graph}"));
        }
        Ok(())
    }
}

fn gone(e: &str) -> WireError {
    WireError {
        code: ErrorCode::Protocol,
        message: e.to_owned(),
    }
}

/// The service over real weida exchanges: create a shell, see its output,
/// resize it, let a viewer in, take control, drop every stream and find the
/// child still there on the next attach. Loopback QUIC with mTLS both ways,
/// as the runner binds it.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use weida::{ClientTls, EndpointAddr, Identity, Runtime, RuntimeConfig, ServerTls, Trust};
    use zeughaus_mux::input::Key;
    use zeughaus_mux::{
        AttachTarget, Axis, KeyInput, KeyKind, Modifiers, NamedKey, SurfaceRef, TerminalEvent,
    };

    struct Client {
        requester: weida::Requester,
        id: ClientInstanceId,
    }

    struct Exchange {
        request: weida::OutgoingTransfer,
        reply: weida::IncomingTransfer,
    }

    impl Exchange {
        async fn send(&mut self, message: Message, request_id: u64) {
            write_frame(&mut self.request, &message, request_id)
                .await
                .expect("write");
        }

        async fn next(&mut self) -> Message {
            tokio::time::timeout(Duration::from_secs(20), read_frame(&mut self.reply))
                .await
                .expect("a frame within 20 s")
                .expect("a readable frame")
                .1
        }
    }

    async fn open(client: &Client, first: Message) -> Exchange {
        let (mut request, reply) = client
            .requester
            .open(TransferMeta::default())
            .await
            .expect("open");
        write_frame(&mut request, &first, 0)
            .await
            .expect("first frame");
        let reply = reply.recv().await.expect("reply half");
        Exchange { request, reply }
    }

    async fn setup() -> (MuxService, weida::Runtime, weida::Binding, String, Identity) {
        let server_identity = Identity::generate_for(["127.0.0.1"]).expect("identity");
        let client_identity = Identity::generate_for(["client"]).expect("identity");
        let runtime = Runtime::new(RuntimeConfig::default()).expect("runtime");
        let listener = runtime.listener();
        let tls = ServerTls::new(server_identity.clone())
            .require_client(Trust::by_address().and_pin(client_identity.fingerprint().unwrap()));
        let binding = listener
            .bind_quic(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), tls)
            .await
            .expect("bind");
        let replier = listener.replier("/mux").expect("replier");
        let shell = Profile {
            label: "sh".into(),
            program: Some("/bin/sh".into()),
            args: vec![],
            cwd: None,
            env: vec![("PS1".into(), "$ ".into())],
            scrollback_rows: 100,
        };
        let service = MuxService::new(
            RunnerIncarnation::from_bytes([7; 16]),
            vec![shell],
            TerminalHost::Local,
        );
        tokio::spawn(service.clone().accept(replier));
        let url = EndpointAddr {
            host: "127.0.0.1".into(),
            port: Some(binding.local_addr().port()),
            path: "/mux".into(),
            peer: Some(server_identity.fingerprint().unwrap()),
        }
        .to_string();
        (service, runtime, binding, url, client_identity)
    }

    async fn client(runtime: &Runtime, url: &str, identity: &Identity, seed: u8) -> Client {
        let requester =
            runtime.requester(ClientTls::new(Trust::by_address()).with_identity(identity.clone()));
        requester.connect(url).await.expect("connect");
        Client {
            requester,
            id: ClientInstanceId::from_bytes([seed; 16]),
        }
    }

    fn hello(client: &Client) -> Message {
        Message::ControlAttach(ControlAttach {
            hello: ClientHello {
                major: MAJOR,
                minor: MINOR,
                client: client.id,
                capabilities: vec![],
                known_incarnation: None,
                known_revision: None,
            },
        })
    }

    fn row_text(rows: &[zeughaus_mux::RowData]) -> String {
        rows.iter()
            .flat_map(|r| r.spans.iter().map(|s| s.text.as_str()))
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Waits for deltas until `pred` holds on one, collecting events.
    async fn until(
        exchange: &mut Exchange,
        mut pred: impl FnMut(&zeughaus_mux::TerminalDelta) -> bool,
    ) -> Vec<TerminalEvent> {
        let mut events = Vec::new();
        for _ in 0..200 {
            match exchange.next().await {
                Message::TerminalDelta(delta) => {
                    events.extend(delta.ordered_events.iter().cloned());
                    if pred(&delta) {
                        return events;
                    }
                }
                other => panic!("unexpected {:?}", other.kind()),
            }
        }
        panic!("no delta satisfied the predicate");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shell_is_created_driven_shared_and_survives_its_viewers() {
        let (service, runtime, _binding, url, identity) = setup().await;
        let alice = client(&runtime, &url, &identity, 1).await;

        // Attach: the default workspace, no terminals.
        let mut control = open(&alice, hello(&alice)).await;
        let Message::ControlAttached(attached) = control.next().await else {
            panic!("expected ControlAttached");
        };
        assert_eq!(attached.workspace.tabs().count(), 0);
        assert!(attached.heads.is_empty());
        assert_eq!(
            attached.hello.principal,
            identity.fingerprint().unwrap().to_string()
        );

        // A new terminal tab: reply, then the snapshot that shows it.
        control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::NewTerminalTab {
                        profile: ProfileId::DEFAULT,
                    },
                }),
                1,
            )
            .await;
        let mut terminal = None;
        for _ in 0..2 {
            match control.next().await {
                Message::CommandReply(reply) => {
                    assert_eq!(reply.request, RequestId(1));
                    assert!(matches!(
                        reply.outcome,
                        CommandOutcome::Applied { revision: 2 }
                    ));
                }
                Message::WorkspaceSnapshot(snapshot) => {
                    assert_eq!(snapshot.revision, 2);
                    terminal = snapshot.terminals().next();
                }
                other => panic!("unexpected {:?}", other.kind()),
            }
        }
        let terminal = terminal.expect("the snapshot names the terminal");

        // The same request id again is answered, not applied again.
        control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::NewTerminalTab {
                        profile: ProfileId::DEFAULT,
                    },
                }),
                1,
            )
            .await;
        assert!(matches!(
            control.next().await,
            Message::CommandReply(CommandReply {
                outcome: CommandOutcome::Applied { revision: 2 },
                ..
            })
        ));

        // Attach to the terminal, type, see the echo.
        let mut term = open(
            &alice,
            Message::TerminalAttach(TerminalAttach {
                client: alice.id,
                terminal,
                known: None,
                size: Dimensions { cols: 40, rows: 10 },
            }),
        )
        .await;
        let Message::TerminalAttached(TerminalAttached { head: Some(head) }) = term.next().await
        else {
            panic!("expected a head");
        };
        assert_eq!(head.terminal, terminal);
        term.send(
            Message::TerminalCommand(TerminalCommand::Text {
                serial: 1,
                text: "echo mux-ok\n".into(),
            }),
            0,
        )
        .await;
        let mut seen = String::new();
        until(&mut term, |delta| {
            seen.push_str(&row_text(&delta.row_replacements));
            seen.contains("mux-ok") && delta.input_serial_ack >= 1
        })
        .await;
        assert!(
            service.controls(terminal, alice.id),
            "the first typist holds the lease"
        );

        // Resize as the controller.
        term.send(
            Message::TerminalCommand(TerminalCommand::Resize(Dimensions { cols: 60, rows: 12 })),
            0,
        )
        .await;
        until(&mut term, |delta| {
            delta.dimensions == Some(Dimensions { cols: 60, rows: 12 })
        })
        .await;

        // A viewer: sees the screen, cannot type, can take control.
        let bob = client(&runtime, &url, &identity, 2).await;
        let mut bob_control = open(&bob, hello(&bob)).await;
        let Message::ControlAttached(attached) = bob_control.next().await else {
            panic!("expected ControlAttached");
        };
        assert_eq!(attached.heads.len(), 1);
        assert!(row_text(&attached.heads[0].rows).contains("mux-ok"));
        assert_eq!(
            attached.heads[0].controller.as_ref().map(|c| c.client),
            Some(alice.id)
        );
        let mut bob_term = open(
            &bob,
            Message::TerminalAttach(TerminalAttach {
                client: bob.id,
                terminal,
                known: None,
                size: Dimensions { cols: 60, rows: 12 },
            }),
        )
        .await;
        let Message::TerminalAttached(TerminalAttached { head: Some(_) }) = bob_term.next().await
        else {
            panic!("expected a head");
        };
        bob_term
            .send(
                Message::TerminalCommand(TerminalCommand::Text {
                    serial: 1,
                    text: "echo viewer-typed\n".into(),
                }),
                0,
            )
            .await;
        // Nothing of Bob's reaches the shell: prove it by typing as Alice
        // afterwards and checking what arrived first.
        term.send(
            Message::TerminalCommand(TerminalCommand::Text {
                serial: 2,
                text: "echo alice-again\n".into(),
            }),
            0,
        )
        .await;
        let mut seen = String::new();
        until(&mut term, |delta| {
            seen.push_str(&row_text(&delta.row_replacements));
            seen.contains("alice-again")
        })
        .await;
        assert!(
            !seen.contains("viewer-typed"),
            "a viewer's input is dropped: {seen}"
        );

        // Semantic keys, not text: what a widget sends per keystroke.
        let mut serial = 3;
        for ch in "echo key-ok".chars() {
            term.send(
                Message::TerminalCommand(TerminalCommand::Key {
                    serial,
                    input: KeyInput {
                        key: Key::Char(ch),
                        modifiers: Modifiers::default(),
                        kind: KeyKind::Press,
                    },
                }),
                0,
            )
            .await;
            serial += 1;
        }
        term.send(
            Message::TerminalCommand(TerminalCommand::Key {
                serial,
                input: KeyInput {
                    key: Key::Named(NamedKey::Enter),
                    modifiers: Modifiers::default(),
                    kind: KeyKind::Press,
                },
            }),
            0,
        )
        .await;
        let mut seen = String::new();
        until(&mut term, |delta| {
            seen.push_str(&row_text(&delta.row_replacements));
            seen.contains("key-ok") && delta.input_serial_ack >= serial
        })
        .await;

        // A burst that scrolls: rows that left the screen between two deltas
        // were never sent, and a fetch is how a viewer gets them.
        term.send(
            Message::TerminalCommand(TerminalCommand::Text {
                serial: serial + 1,
                text: "seq 1 100\n".into(),
            }),
            0,
        )
        .await;
        let mut visible = None;
        until(&mut term, |delta| {
            if row_text(&delta.row_replacements).contains("100") {
                visible = delta.visible;
                true
            } else {
                false
            }
        })
        .await;
        let visible = visible.expect("a delta names the visible rows");
        let mut fetch = open(
            &alice,
            Message::RowFetch(RowFetch {
                terminal,
                epoch: 1,
                range: zeughaus_mux::StableRange {
                    start: (visible.start - 40).max(0),
                    end: visible.start,
                },
                generation: 7,
            }),
        )
        .await;
        let Message::RowPage(page) = fetch.next().await else {
            panic!("expected a page");
        };
        assert_eq!(page.generation, 7);
        assert!(!page.rows.is_empty(), "the rows above the screen exist");
        let text = row_text(&page.rows);
        assert!(
            text.contains("|5|") || text.contains("50"),
            "the page holds the burst's earlier lines: {text}"
        );

        bob_control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::TakeControl { terminal },
                }),
                1,
            )
            .await;
        assert!(matches!(bob_control.next().await, Message::CommandReply(_)));
        let events = until(&mut term, |delta| {
            delta.ordered_events.iter().any(
                |e| matches!(e, TerminalEvent::ControllerChanged(Some(c)) if c.client == bob.id),
            )
        })
        .await;
        assert!(!events.is_empty());
        assert!(service.controls(terminal, bob.id));

        // Everyone leaves; the child keeps running; the next attach finds
        // the screen as it was.
        drop(term);
        drop(bob_term);
        drop(control);
        drop(bob_control);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let session = service
            .session(terminal)
            .expect("the session outlives its viewers");
        assert!(
            session.exit().is_none(),
            "the child was not killed by a detach"
        );

        let carol = client(&runtime, &url, &identity, 3).await;
        let mut control = open(&carol, hello(&carol)).await;
        let Message::ControlAttached(attached) = control.next().await else {
            panic!("expected ControlAttached");
        };
        assert!(row_text(&attached.heads[0].rows).contains("100"));
        // Bob's lease is within its grace, and Carol proves the same identity
        // Bob did -- the same user's restarted editor. Her first keystroke
        // takes the shell over rather than waiting the grace out.
        assert!(service.controls(terminal, bob.id));
        let mut carol_term = open(
            &carol,
            Message::TerminalAttach(TerminalAttach {
                client: carol.id,
                terminal,
                known: None,
                size: Dimensions { cols: 60, rows: 12 },
            }),
        )
        .await;
        let _ = carol_term.next().await;
        carol_term
            .send(
                Message::TerminalCommand(TerminalCommand::Text {
                    serial: 1,
                    text: "echo carol-back\n".into(),
                }),
                0,
            )
            .await;
        let mut seen = String::new();
        until(&mut carol_term, |delta| {
            seen.push_str(&row_text(&delta.row_replacements));
            seen.contains("carol-back")
        })
        .await;
        assert!(service.controls(terminal, carol.id));
        drop(carol_term);

        // Closing the pane kills the child and leaves no tab behind.
        let pane = attached.workspace.tabs().next().unwrap().root.leaves()[0].0;
        control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::ClosePane { pane },
                }),
                1,
            )
            .await;
        for _ in 0..2 {
            match control.next().await {
                Message::CommandReply(reply) => {
                    assert!(matches!(reply.outcome, CommandOutcome::Applied { .. }));
                }
                Message::WorkspaceSnapshot(snapshot) => {
                    assert_eq!(snapshot.tabs().count(), 0);
                }
                other => panic!("unexpected {:?}", other.kind()),
            }
        }
        for _ in 0..50 {
            if session.exit().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(matches!(
            session.exit(),
            Some(zeughaus_mux::ExitState::Killed)
        ));
        assert!(service.session(terminal).is_none());

        // A stranger's exchange kind, and a key that means nothing to the
        // graph: both refused in words, never a panic.
        let mut bad = open(
            &carol,
            Message::TerminalCommand(TerminalCommand::Key {
                serial: 1,
                input: KeyInput {
                    key: Key::Named(NamedKey::Enter),
                    modifiers: Modifiers::default(),
                    kind: KeyKind::Press,
                },
            }),
        )
        .await;
        assert!(matches!(
            bad.next().await,
            Message::Error(WireError {
                code: ErrorCode::Protocol,
                ..
            })
        ));
        let mut buf = [0u8; 1];
        assert_eq!(
            bad.reply.read(&mut buf).await.ok(),
            Some(0),
            "the exchange is finished"
        );

        // A graph opened on the reconnected control is a pane like any other.
        control
            .send(
                Message::Command(Command {
                    request: RequestId(2),
                    command: TopologyCommand::OpenGraph { graph: 7 },
                }),
                2,
            )
            .await;
        let mut graph_pane = None;
        for _ in 0..2 {
            if let Message::WorkspaceSnapshot(s) = control.next().await {
                graph_pane = s
                    .tabs()
                    .flat_map(|tab| tab.root.leaves())
                    .find(|(_, surface)| *surface == SurfaceRef::Graph(7))
                    .map(|(pane, _)| pane);
            }
        }
        let graph_pane = graph_pane.expect("the snapshot shows the graph");
        control
            .send(
                Message::Command(Command {
                    request: RequestId(3),
                    command: TopologyCommand::SplitWithTerminal {
                        pane: graph_pane,
                        axis: Axis::Horizontal,
                        profile: ProfileId::DEFAULT,
                    },
                }),
                3,
            )
            .await;
        let mut split_seen = false;
        for _ in 0..2 {
            if let Message::WorkspaceSnapshot(s) = control.next().await {
                split_seen = s.tabs().next().unwrap().root.leaf_count() == 2;
            }
        }
        assert!(split_seen);
    }

    /// The plan's acceptance numbers, measured on loopback: a fresh dial plus
    /// attach (QUIC, TLS, HELLO, one control exchange), a warm attach on the
    /// pooled connection, and a keystroke's round trip to the delta that
    /// echoes it. Ignored in the gate: it is a measurement, not a contract,
    /// and it is meant for `cargo test --release -- --ignored perf`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn perf_attach_and_keystroke_latency() {
        fn percentiles(samples: &mut [Duration]) -> (Duration, Duration) {
            samples.sort();
            let p50 = samples[samples.len() / 2];
            let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
            (p50, p95)
        }

        let (_service, runtime, _binding, url, identity) = setup().await;

        // Fresh dial: a new requester each time, so the pool cannot reuse.
        let mut fresh = Vec::new();
        for i in 0..20u8 {
            // A runtime of its own per dial: the pool keys on the trust
            // configuration, and an equal one would hand back the warm
            // connection.
            let fresh_runtime = Runtime::new(RuntimeConfig::default()).expect("runtime");
            let started = Instant::now();
            let client = client(&fresh_runtime, &url, &identity, 10 + i).await;
            let mut control = open(&client, hello(&client)).await;
            let Message::ControlAttached(_) = control.next().await else {
                panic!("expected ControlAttached");
            };
            fresh.push(started.elapsed());
        }
        let (p50, p95) = percentiles(&mut fresh);
        eprintln!("fresh dial + attach: p50 {p50:?} p95 {p95:?}");

        // Warm attach: one requester, one pooled connection, many exchanges.
        let alice = client(&runtime, &url, &identity, 1).await;
        let mut warm = Vec::new();
        for _ in 0..50 {
            let started = Instant::now();
            let mut control = open(&alice, hello(&alice)).await;
            let Message::ControlAttached(_) = control.next().await else {
                panic!("expected ControlAttached");
            };
            warm.push(started.elapsed());
        }
        let (p50, p95) = percentiles(&mut warm);
        eprintln!("warm attach: p50 {p50:?} p95 {p95:?}");

        // A terminal, then keystrokes: the time from sending a key to the
        // delta that shows its echo.
        let mut control = open(&alice, hello(&alice)).await;
        let _ = control.next().await;
        control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::NewTerminalTab {
                        profile: ProfileId::DEFAULT,
                    },
                }),
                1,
            )
            .await;
        let mut terminal = None;
        for _ in 0..2 {
            if let Message::WorkspaceSnapshot(s) = control.next().await {
                terminal = s.terminals().next();
            }
        }
        let terminal = terminal.expect("terminal");
        let mut term = open(
            &alice,
            Message::TerminalAttach(TerminalAttach {
                client: alice.id,
                terminal,
                known: None,
                size: Dimensions { cols: 80, rows: 24 },
            }),
        )
        .await;
        let _ = term.next().await;
        // Let the shell print its prompt before timing anything.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while tokio::time::timeout(Duration::from_millis(200), term.next())
            .await
            .is_ok()
        {}

        let mut keys = Vec::new();
        for i in 0..50u64 {
            let ch = char::from(b'a' + (i % 26) as u8);
            // A person types with gaps well past the coalescing window;
            // back-to-back keys would measure the burst pacing instead.
            tokio::time::sleep(Duration::from_millis(40)).await;
            let started = Instant::now();
            term.send(
                Message::TerminalCommand(TerminalCommand::Key {
                    serial: i + 1,
                    input: KeyInput {
                        key: Key::Char(ch),
                        modifiers: Modifiers::default(),
                        kind: KeyKind::Press,
                    },
                }),
                0,
            )
            .await;
            until(&mut term, |delta| delta.input_serial_ack > i).await;
            keys.push(started.elapsed());
        }
        let (p50, p95) = percentiles(&mut keys);
        eprintln!("keystroke to echoed delta: p50 {p50:?} p95 {p95:?}");

        // Codec cost of what an attach carries.
        let head = _service
            .session(terminal)
            .expect("session")
            .head(HEAD_ROWS_ABOVE);
        let started = Instant::now();
        let bytes = Message::TerminalHead(head.clone())
            .encode(0)
            .expect("encode");
        let encode = started.elapsed();
        let started = Instant::now();
        let header = zeughaus_mux::FrameHeader::decode(
            bytes[..zeughaus_mux::FrameHeader::LEN].try_into().unwrap(),
        )
        .unwrap();
        let _ = Message::decode(header.kind, &bytes[zeughaus_mux::FrameHeader::LEN..]).unwrap();
        let decode = started.elapsed();
        eprintln!(
            "head of {} rows: {} bytes, encode {encode:?}, decode {decode:?}",
            head.rows.len(),
            bytes.len()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_the_runner_owns_is_listed_attached_and_only_closed_on_demand() {
        let (service, runtime, _binding, url, identity) = setup().await;
        let job = service
            .spawn_owned(
                Profile {
                    label: "job".into(),
                    program: Some("/bin/sh".into()),
                    args: vec!["-c".into(), "sleep 30".into()],
                    cwd: None,
                    env: vec![],
                    scrollback_rows: 100,
                },
                None,
                None,
                OwnedPlacement::Detached,
            )
            .expect("start the job's process");

        let alice = client(&runtime, &url, &identity, 1).await;
        let mut control = open(&alice, hello(&alice)).await;
        let Message::ControlAttached(attached) = control.next().await else {
            panic!("expected ControlAttached");
        };
        assert_eq!(
            attached
                .workspace
                .detached
                .iter()
                .map(|d| d.terminal)
                .collect::<Vec<_>>(),
            vec![job],
            "an owned terminal is listed before any pane shows it"
        );
        assert_eq!(attached.workspace.terminals().count(), 0);

        control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::AttachTerminal {
                        terminal: job,
                        target: AttachTarget::NewTab,
                    },
                }),
                1,
            )
            .await;
        let mut shown = false;
        for _ in 0..2 {
            match control.next().await {
                Message::CommandReply(reply) => {
                    assert!(matches!(reply.outcome, CommandOutcome::Applied { .. }));
                }
                Message::WorkspaceSnapshot(snapshot) => {
                    assert!(snapshot.detached.is_empty());
                    assert_eq!(snapshot.terminals().collect::<Vec<_>>(), vec![job]);
                    shown = true;
                }
                other => panic!("unexpected {:?}", other.kind()),
            }
        }
        assert!(shown, "the snapshot after an attach shows the terminal");

        service.close_terminal(job).expect("close the job terminal");
        assert!(
            service.session(job).is_none(),
            "a closed terminal is forgotten"
        );
        assert!(service.close_terminal(job).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_title_the_child_sets_renames_its_tab_without_a_terminal_stream() {
        let (service, runtime, _binding, url, identity) = setup().await;
        let alice = client(&runtime, &url, &identity, 1).await;
        let mut control = open(&alice, hello(&alice)).await;
        let Message::ControlAttached(_) = control.next().await else {
            panic!("expected ControlAttached");
        };
        control
            .send(
                Message::Command(Command {
                    request: RequestId(1),
                    command: TopologyCommand::NewTerminalTab {
                        profile: ProfileId::DEFAULT,
                    },
                }),
                1,
            )
            .await;
        let mut terminal = None;
        for _ in 0..2 {
            if let Message::WorkspaceSnapshot(snapshot) = control.next().await {
                terminal = snapshot.terminals().next();
            }
        }
        let terminal = terminal.expect("the snapshot names the terminal");

        // Nobody attaches to the terminal: the title has to arrive on the
        // control exchange alone.
        service
            .session(terminal)
            .expect("session")
            .apply(&TerminalCommand::Text {
                serial: 1,
                text: "printf '\\033]0;renamed\\007'\n".into(),
            })
            .expect("type");
        for _ in 0..20 {
            match control.next().await {
                Message::WorkspaceSnapshot(snapshot)
                    if snapshot
                        .tabs()
                        .next()
                        .is_some_and(|tab| tab.title == "renamed") =>
                {
                    let tab = snapshot.tabs().next().unwrap().id;
                    assert!(matches!(
                        service.structural(&TopologyCommand::CloseTab { tab }),
                        CommandOutcome::Applied { .. }
                    ));
                    return;
                }
                Message::WorkspaceSnapshot(_) => {}
                other => panic!("unexpected {:?}", other.kind()),
            }
        }
        panic!("no snapshot carried the new title");
    }

    #[test]
    fn a_graph_tab_is_named_after_its_node_and_only_a_known_graph_opens() {
        let service = MuxService::new(
            RunnerIncarnation::from_bytes([7; 16]),
            Vec::new(),
            TerminalHost::Local,
        );
        // Before the first sync nothing is known, so nothing is refused.
        assert!(matches!(
            service.structural(&TopologyCommand::OpenGraph { graph: 5 }),
            CommandOutcome::Applied { .. }
        ));
        service.sync_graphs(GraphSync {
            owned: vec![1],
            names: HashMap::from([(1, "Pipeline".to_owned())]),
            exists: HashSet::from([1]),
        });
        let snapshot = service.snapshot();
        assert_eq!(
            snapshot
                .tabs()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["Pipeline"],
            "the missing graph's tab went, the runner's own came"
        );
        assert_eq!(*service.inner.revision.borrow(), snapshot.revision);
        assert!(matches!(
            service.structural(&TopologyCommand::OpenGraph { graph: 5 }),
            CommandOutcome::Refused { .. }
        ));
    }
}
