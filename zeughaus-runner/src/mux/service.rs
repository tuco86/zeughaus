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

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use weida::{IncomingMeta, IncomingRequest, PeerIdentity, Replier, TransferMeta};
use zeughaus_mux::message::{Capability, Command, ErrorCode};
use zeughaus_mux::workspace::ProfileId;
use zeughaus_mux::{
    ClientHello, ClientInstanceId, CommandOutcome, CommandReply, ControlAttach, Controller,
    Dimensions, MAJOR, MINOR, Message, RequestId, RowFetch, RowPage, RunnerIncarnation,
    ServerHello, TerminalAttach, TerminalAttached, TerminalCommand, TerminalId, TopologyCommand,
    WireError, WorkspaceSnapshot,
};
use zeughaus_terminal::{Profile, Session};

use super::frames::{ReadError, read_frame, write_frame};
use super::workspace::Workspace;

/// Scrollback rows sent with a head, above the visible ones: enough that a
/// wheel notch or two needs no fetch, bounded so an attach stays small.
const HEAD_ROWS_ABOVE: usize = 128;

/// How long after a client vanishes its leases are held for it, so a network
/// blink does not make its terminals read-only.
const LEASE_GRACE: Duration = Duration::from_secs(10);

/// The render cadence a subscriber coalesces at: a delta at most this often
/// per terminal per client, whatever the child prints.
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

struct Inner {
    incarnation: RunnerIncarnation,
    profiles: Vec<Profile>,
    workspace: Mutex<Workspace>,
    sessions: Mutex<HashMap<TerminalId, Session>>,
    leases: Mutex<HashMap<TerminalId, Lease>>,
    clients: Mutex<HashMap<ClientInstanceId, ClientState>>,
    next_terminal: AtomicU64,
    /// The workspace revision, for control exchanges to wake on.
    revision: watch::Sender<u64>,
}

/// The service handle. Cheap to clone; one per runner.
#[derive(Clone)]
pub struct MuxService {
    inner: Arc<Inner>,
}

impl MuxService {
    pub fn new(incarnation: RunnerIncarnation, profiles: Vec<Profile>) -> MuxService {
        let profiles = if profiles.is_empty() {
            vec![Profile::default_shell()]
        } else {
            profiles
        };
        let (revision, _) = watch::channel(1);
        MuxService {
            inner: Arc::new(Inner {
                incarnation,
                profiles,
                workspace: Mutex::new(Workspace::new(incarnation)),
                sessions: Mutex::new(HashMap::new()),
                leases: Mutex::new(HashMap::new()),
                clients: Mutex::new(HashMap::new()),
                next_terminal: AtomicU64::new(1),
                revision,
            }),
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
        let attached = self.attached(&hello, &principal);
        write_frame(reply, &Message::ControlAttached(attached), 0)
            .await
            .map_err(|e| gone(&e))?;
        let mut sent_revision = *revision.borrow_and_update();

        let mut canceled = std::pin::pin!(canceled);
        let result = loop {
            tokio::select! {
                () = &mut canceled => break Ok(()),
                changed = revision.changed() => {
                    if changed.is_err() {
                        break Ok(());
                    }
                    let current = *revision.borrow_and_update();
                    if current != sent_revision {
                        sent_revision = current;
                        let snapshot = self.snapshot();
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
        let mut head = match attach.known {
            Some((1, seq)) if seq == current => None,
            _ => Some(session.head(HEAD_ROWS_ABOVE)),
        };
        // Serials are per client and per exchange: a fresh attach starts
        // its count at zero, whatever the engine has applied for others.
        if let Some(head) = head.as_mut() {
            head.input_serial_ack = 0;
        }
        let mut acked_serial = 0u64;
        let mut sent_seq = head.as_ref().map_or(current, |h| h.seq);
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
        loop {
            tokio::select! {
                () = &mut canceled => break Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        break Ok(());
                    }
                    // Coalesce: whatever else arrives in the next few
                    // milliseconds goes into the same delta.
                    tokio::time::sleep(COALESCE).await;
                    changes.borrow_and_update();
                    let mut delta = session.delta_since(sent_seq);
                    delta.input_serial_ack = acked_serial;
                    sent_seq = delta.to_seq;
                    if let Err(e) = write_frame(reply, &Message::TerminalDelta(delta), 0).await {
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
        if fetch.epoch != 1 {
            return Err(WireError {
                code: ErrorCode::Stale,
                message: "unknown epoch".to_owned(),
            });
        }
        let (seq, first_retained, rows) = session.rows(fetch.range);
        let page = RowPage {
            terminal: fetch.terminal,
            epoch: 1,
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

    fn detach(&self, client: ClientInstanceId) {
        let now = Instant::now();
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        for lease in leases.values_mut() {
            if lease.client == client && lease.detached_at.is_none() {
                lease.detached_at = Some(now);
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
        let mut snapshot = workspace.snapshot(&|t| sessions.get(&t).map(|s| s.title()));
        // Heads may carry a controller the workspace does not know; the
        // snapshot is structure only. Nothing to merge.
        snapshot.incarnation = self.inner.incarnation;
        snapshot
    }

    fn session(&self, terminal: TerminalId) -> Option<Session> {
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
    /// does (or the holder's grace ran out) and it takes it now.
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
        let mut spawned: Vec<(TerminalId, Session)> = Vec::new();
        let mut spawn = |profile: ProfileId| -> Result<TerminalId, String> {
            let profile = self
                .inner
                .profiles
                .get(profile.0 as usize)
                .ok_or_else(|| format!("no profile {}", profile.0))?;
            let id = TerminalId(self.inner.next_terminal.fetch_add(1, Ordering::Relaxed));
            let session = Session::spawn(id, profile, Dimensions { cols: 80, rows: 24 })
                .map_err(|e| format!("cannot start {}: {e}", profile.label))?;
            spawned.push((id, session));
            Ok(id)
        };
        let result = {
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            workspace
                .apply(command, &mut spawn)
                .map(|applied| (workspace.revision(), applied))
        };
        match result {
            Ok((revision, applied)) => {
                {
                    let mut sessions = self
                        .inner
                        .sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    for (id, session) in spawned {
                        sessions.insert(id, session);
                    }
                    let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
                    for id in &applied.killed {
                        leases.remove(id);
                        if let Some(session) = sessions.remove(id) {
                            session.kill();
                        }
                    }
                }
                self.inner.revision.send_replace(revision);
                CommandOutcome::Applied { revision }
            }
            Err(reason) => {
                // A spawn that succeeded before the command failed is a
                // child nobody shows: end it.
                for (_, session) in spawned {
                    session.kill();
                }
                CommandOutcome::Refused { reason }
            }
        }
    }
}

/// The name the runner shows others for a peer: the fingerprint it proved.
fn principal_of(meta: &IncomingMeta) -> Option<String> {
    match &meta.peer {
        Some(PeerIdentity::Key(fp)) => Some(fp.to_string()),
        Some(other) => Some(format!("{other:?}")),
        None => None,
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
        Axis, KeyInput, Modifiers, NamedKey, PaneId, PaneNode, SurfaceRef, TerminalEvent,
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
        let service = MuxService::new(RunnerIncarnation::from_bytes([7; 16]), vec![shell]);
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
        assert_eq!(attached.workspace.tabs.len(), 1);
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
        // Bob's lease is within its grace: Carol cannot type yet.
        assert!(service.controls(terminal, bob.id));

        // Closing the pane kills the child and answers with the graph alone.
        let pane = attached.workspace.tabs[1].root.leaves()[0].0;
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
                    assert_eq!(snapshot.tabs.len(), 1);
                    assert!(matches!(
                        snapshot.tabs[0].root,
                        PaneNode::Leaf {
                            surface: SurfaceRef::Graph,
                            ..
                        }
                    ));
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

        // Splitting the graph pane still works on the reconnected control.
        control
            .send(
                Message::Command(Command {
                    request: RequestId(2),
                    command: TopologyCommand::SplitWithTerminal {
                        pane: PaneId(1),
                        axis: Axis::Horizontal,
                        profile: ProfileId::DEFAULT,
                    },
                }),
                2,
            )
            .await;
        let mut split_seen = false;
        for _ in 0..2 {
            if let Message::WorkspaceSnapshot(s) = control.next().await {
                split_seen = s.tabs[0].root.leaf_count() == 2;
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
            let started = Instant::now();
            term.send(
                Message::TerminalCommand(TerminalCommand::Key {
                    serial: i + 1,
                    input: KeyInput {
                        key: Key::Char(ch),
                        modifiers: Modifiers::default(),
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
}
