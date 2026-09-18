//! SpacetimeDB client for the shared graph: generated bindings plus the
//! connect/subscribe layer both processes use.
//!
//! - Receive: subscribes to `node`, `edge` and `runtime`; row changes become
//!   [`SyncEvent`]s on a channel the caller drains on its own schedule (the SDK
//!   callbacks run on a background thread, so state is never touched from
//!   there).
//! - Send: helper functions call the module's reducers for local edits and for
//!   announcing where a runtime is reachable.
//!
//! The store carries the graph document and runtime presence, and nothing a
//! pass produces: values, edge traffic, frames and trigger presses travel over
//! weida, straight between the runner and the editors. This is a library
//! because the editor and the headless runtime are separate processes that both
//! speak to the same store.
//!
//! Conflict model: fine-grained reducers, last-writer-wins per row. Node/edge
//! ids are made process-unique at startup (see `NodeId::seed_unique`) so two
//! clients never assign colliding ids. Applying a remote change is guarded by
//! the caller so it does not echo back as a reducer call.
//!
//! Always-on: connecting is required, with no local-only fallback. A missing
//! server is a fatal startup error, not a degraded mode.

pub mod module_bindings;

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey, credentials};
use zeughaus_core::{EdgeData, NodeData};

use crate::module_bindings::{
    DbConnection, Edge, EdgeTableAccess, Node, NodeTableAccess, RuntimeTableAccess,
    announce_endpoint, connect_edge, create_node, delete_node, disconnect_edge, join_runtime,
    move_node, set_node_params,
};

pub const DEFAULT_PORT: u16 = 3000;
pub const DEFAULT_SESSION: &str = "zeughaus";

/// A change observed in the shared store, to be applied to the editor.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    NodeUpsert(NodeData),
    NodeRemove(u64),
    EdgeInsert(EdgeData),
    EdgeRemove(u64),
    /// The set of connected runtimes changed, so who owns execution may have
    /// changed with it. Carries no payload: the editor recomputes ownership from
    /// the client cache, which is the authority.
    RuntimesChanged,
    /// The first subscription snapshot has been delivered. Everything before it
    /// is existing state rather than something that just happened.
    SubscriptionApplied,
    /// The connection is up: this client is subscribed and its edits reach the
    /// store again. Also reported for the first connection, so a caller has
    /// one path for both.
    Connected,
    /// The connection is gone. Every edit made from here on is local-only
    /// until a [`SyncEvent::Connected`] follows, which is a thing only the
    /// caller can say on screen -- and a runner must stop executing, because
    /// the store has already dropped its `runtime` row and handed the graph to
    /// someone else.
    Disconnected,
}

fn to_node_data(n: &Node) -> NodeData {
    NodeData {
        id: n.id,
        type_id: n.type_id.clone(),
        display_name: n.display_name.clone(),
        x: n.x,
        y: n.y,
        // params are stored as a JSON array of [name, value] pairs.
        params: serde_json::from_str(&n.params).unwrap_or_default(),
        parent: n.parent,
    }
}

fn to_edge_data(e: &Edge) -> EdgeData {
    EdgeData {
        id: e.id,
        from_node: e.from_node,
        from_pin: e.from_pin.clone(),
        to_node: e.to_node,
        to_pin: e.to_pin.clone(),
    }
}

/// What a client is here for.
///
/// Only a [`Role::Runtime`] registers in the `runtime` table, because that table
/// answers "who executes the graph". An editor that registered would be elected
/// to run a graph it has no executor for, and a `spacetime sql` connection --
/// also a client -- would do the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Executes the graph and publishes results.
    Runtime,
    /// Edits the graph and displays published results.
    Viewer,
}

/// Connects, wires row-change callbacks into a channel, subscribes to the
/// shared tables, and spawns the background message loop. Returns the live
/// connection (kept alive by the caller) and the receiving end of the event
/// channel.
///
/// One connection, once. A caller that has to survive the store going away
/// wants [`Store`], which is this function plus the retry; this is the
/// primitive both take.
pub fn connect(
    uri: &str,
    module: &str,
    role: Role,
) -> Result<(DbConnection, Receiver<SyncEvent>), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let conn = build(uri, module, role, tx)?;
    Ok((conn, rx))
}

/// Builds one connection, reporting everything it observes on `tx`.
fn build(
    uri: &str,
    module: &str,
    role: Role,
    tx: Sender<SyncEvent>,
) -> Result<DbConnection, String> {
    // The identity is the token: it says WHO this process is, and once anything
    // in the store is owned by an identity, a client that came back as a
    // different one could not touch what it wrote before. Presence is a
    // separate question -- a `runtime` row belongs to a connection, so a
    // reconnect joins as a new row with a fresh ownership sequence.
    let store_key = credential_key(uri, module);
    let saved = credentials::File::new(&store_key)
        .load()
        .unwrap_or_else(|e| {
            eprintln!("[stdb] cannot read the saved token: {e}");
            None
        });

    let connected = tx.clone();
    let lost = tx.clone();
    let failed = tx.clone();
    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(module)
        .with_token(saved)
        .on_connect(move |ctx, identity, token| {
            eprintln!("[stdb] connected as {identity:?} ({role:?})");
            // Saved on every connect rather than only the first: the store
            // issues the token, and the one it just issued is the one a
            // restart has to come back with.
            if let Err(e) = credentials::File::new(&store_key).save(token) {
                eprintln!("[stdb] cannot save the token: {e}");
            }
            // Registering happens here rather than after `connect` returns:
            // a reducer call needs the connection to be established, and this
            // callback is the first point where it is.
            if role == Role::Runtime
                && let Err(e) = ctx.reducers.join_runtime()
            {
                eprintln!("[stdb] join_runtime failed: {e}");
            }
            send(&connected, SyncEvent::Connected);
        })
        .on_connect_error(move |_ctx, err| {
            eprintln!("[stdb] connect error: {err}");
            send(&failed, SyncEvent::Disconnected);
        })
        .on_disconnect(move |_ctx, err| {
            match err {
                Some(e) => eprintln!("[stdb] disconnected: {e}"),
                None => eprintln!("[stdb] disconnected"),
            }
            // The one report of it: a client that keeps editing against a dead
            // connection loses every edit it makes, and only the caller can
            // say so on screen.
            send(&lost, SyncEvent::Disconnected);
        })
        .build()
        .map_err(|e| format!("build failed: {e}"))?;

    // Forward table changes onto the channel. Self-originated changes are echoed
    // back here too; the editor applies them idempotently.
    let t = tx.clone();
    conn.db
        .node()
        .on_insert(move |_ctx, n| send(&t, SyncEvent::NodeUpsert(to_node_data(n))));
    let t = tx.clone();
    conn.db
        .node()
        .on_update(move |_ctx, _old, n| send(&t, SyncEvent::NodeUpsert(to_node_data(n))));
    let t = tx.clone();
    conn.db
        .node()
        .on_delete(move |_ctx, n| send(&t, SyncEvent::NodeRemove(n.id)));
    let t = tx.clone();
    conn.db
        .edge()
        .on_insert(move |_ctx, e| send(&t, SyncEvent::EdgeInsert(to_edge_data(e))));
    let t = tx.clone();
    conn.db
        .edge()
        .on_delete(move |_ctx, e| send(&t, SyncEvent::EdgeRemove(e.id)));
    let t = tx.clone();
    conn.db
        .runtime()
        .on_insert(move |_ctx, _r| send(&t, SyncEvent::RuntimesChanged));
    let t = tx.clone();
    conn.db
        .runtime()
        .on_delete(move |_ctx, _r| send(&t, SyncEvent::RuntimesChanged));

    let t = tx.clone();
    conn.subscription_builder()
        .on_applied(move |_ctx| {
            eprintln!("[stdb] subscription applied");
            send(&t, SyncEvent::SubscriptionApplied);
        })
        .on_error(|_ctx, err| eprintln!("[stdb] subscription error: {err}"))
        .subscribe([
            "SELECT * FROM node",
            "SELECT * FROM edge",
            "SELECT * FROM runtime",
        ]);

    conn.run_threaded();
    Ok(conn)
}

/// Where this client's token is kept: one file per store, because an identity
/// issued by one host means nothing to another.
fn credential_key(uri: &str, module: &str) -> String {
    let host: String = uri
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("zeughaus-{host}-{module}")
}

/// A store connection that rebuilds itself.
///
/// The connection is not a thing a process can be handed once and rely on: a
/// host restart, a suspended laptop or a dropped Wi-Fi packet ends it, and the
/// SDK's message loop simply exits. Before this, the owner of the connection
/// kept a dead handle: every reducer call failed into a log line nobody reads,
/// and the process went on as if the store were there.
///
/// So the connection lives here, behind [`Store::conn`] returning an `Option`:
/// "there is no store right now" becomes a state the caller has to handle
/// rather than one it cannot see. [`Store::poll`] is the retry, called from
/// the caller's own loop so nothing reconnects behind its back.
pub struct Store {
    uri: String,
    module: String,
    role: Role,
    /// The channel the caller drains. Kept so a rebuilt connection reports on
    /// the same one -- a reconnect must not need a new receiver.
    tx: Sender<SyncEvent>,
    conn: Option<DbConnection>,
    /// Consecutive failed attempts, for the backoff. Reset by a live
    /// connection, so a host that flaps once does not inherit the delay of an
    /// hour-long outage.
    attempt: u32,
    /// When the next attempt may happen.
    next_attempt: Instant,
}

impl Store {
    /// Connects, and keeps the terms so it can connect again.
    ///
    /// The first connection has to succeed: a session token that names no
    /// reachable store is a startup mistake, and retrying it forever would
    /// only hide it.
    pub fn open(
        uri: &str,
        module: &str,
        role: Role,
    ) -> Result<(Self, Receiver<SyncEvent>), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let conn = build(uri, module, role, tx.clone())?;
        let store = Self {
            uri: uri.to_string(),
            module: module.to_string(),
            role,
            tx,
            conn: Some(conn),
            attempt: 0,
            next_attempt: Instant::now(),
        };
        Ok((store, rx))
    }

    /// The live connection, or `None` while there is none.
    pub fn conn(&self) -> Option<&DbConnection> {
        self.conn.as_ref()
    }

    /// Whether the store is reachable right now.
    pub fn is_live(&self) -> bool {
        self.conn.as_ref().is_some_and(DbContext::is_active)
    }

    /// Rebuilds the connection when it is down and the backoff has elapsed.
    ///
    /// Called once per turn of the caller's loop. A dead connection is dropped
    /// first: its message thread has already exited, and holding it would keep
    /// the socket and answer `is_live` with a lie.
    ///
    /// The attempt itself is synchronous, because the SDK's `build` is: a
    /// refused connection returns at once, but a host that accepts nothing and
    /// answers nothing holds the caller for the system's TCP timeout. That is
    /// why the backoff exists -- one such attempt every few seconds, not one
    /// per turn.
    pub fn poll(&mut self) {
        if self.is_live() {
            self.attempt = 0;
            return;
        }
        self.conn = None;
        if Instant::now() < self.next_attempt {
            return;
        }
        self.attempt += 1;
        match build(&self.uri, &self.module, self.role, self.tx.clone()) {
            Ok(conn) => {
                eprintln!("[stdb] reconnected to {} / {}", self.uri, self.module);
                self.conn = Some(conn);
            }
            Err(e) => {
                let wait = retry_delay(self.attempt);
                eprintln!(
                    "[stdb] cannot reach {} / {}: {e} (retrying in {:.1}s)",
                    self.uri,
                    self.module,
                    wait.as_secs_f32()
                );
                self.next_attempt = Instant::now() + wait;
            }
        }
    }
}

/// Shortest gap before a reconnection attempt.
const RETRY_MIN: Duration = Duration::from_millis(250);

/// Longest gap between two attempts. A store that has been down for an hour is
/// still worth asking every few seconds: the process is otherwise idle, and
/// the first thing a user does after restarting the host is look at an editor.
const RETRY_MAX: Duration = Duration::from_secs(5);

/// How long to wait before attempt number `attempt` (1 is the first).
///
/// Doubling from [`RETRY_MIN`] to [`RETRY_MAX`]: a host that is restarting is
/// back within a second, and one that is gone for the afternoon must not be
/// asked in a tight loop.
pub fn retry_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    (RETRY_MIN * 2u32.saturating_pow(doublings)).min(RETRY_MAX)
}

fn send(tx: &Sender<SyncEvent>, ev: SyncEvent) {
    // The receiver lives for the app's lifetime; a send error just means the
    // editor is shutting down, which we can ignore.
    let _ = tx.send(ev);
}

/// Best-effort outbound LAN ip, for building a session token a remote buddy can
/// reach. Falls back to the loopback address.
pub fn lan_ip() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            // No packets are sent; this just selects the outbound interface.
            sock.connect("8.8.8.8:80")?;
            sock.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Parses a session token `host[:port]/db` (or a bare `db` on localhost) into a
/// connection uri and database name.
pub fn parse_token(token: &str) -> (String, String) {
    match token.split_once('/') {
        Some((host, db)) => {
            let host = if host.contains(':') {
                host.to_string()
            } else {
                format!("{host}:{DEFAULT_PORT}")
            };
            (format!("http://{host}"), db.to_string())
        }
        None => (
            format!("http://127.0.0.1:{DEFAULT_PORT}"),
            token.to_string(),
        ),
    }
}

fn params_json(params: &[(String, String)]) -> String {
    serde_json::to_string(params).unwrap_or_else(|_| "[]".to_string())
}

/// Whether this client owns execution: the runtime with the lowest `seq` runs
/// the graph, a second one is a standby. Only meaningful for a
/// [`Role::Runtime`] client.
///
/// Compared by CONNECTION, not by identity: two runners on one machine share
/// the saved token and are therefore one identity, and by identity both of them
/// would believe they own the graph.
///
/// Read from the client cache rather than remembered, so a runtime leaving
/// hands ownership over without any handshake. While the cache is still empty
/// (before the first subscription applies) nobody owns anything, which keeps a
/// starting runner from executing a graph it has not seen yet.
pub fn is_owner(conn: &DbConnection) -> bool {
    let Some(me) = conn.try_connection_id() else {
        return false;
    };
    conn.db
        .runtime()
        .iter()
        .min_by_key(|r| r.seq)
        .is_some_and(|r| r.connection_id == me)
}

/// How many runtimes are connected. An editor uses this to say whether anything
/// is executing at all: with no runtime, a graph is drawn but nothing runs, and
/// silence is the one thing a user must not have to guess about.
pub fn runtime_count(conn: &DbConnection) -> usize {
    conn.db.runtime().count() as usize
}

/// The pinned URL the executing runtime is reachable at. `None` while no
/// runtime is connected or the owning one serves nothing.
///
/// Reads the OWNING runtime specifically: an editor watching a runtime has to
/// watch the process that is producing values, and a standby produces nothing.
pub fn owner_endpoint(conn: &DbConnection) -> Option<String> {
    let owner = conn.db.runtime().iter().min_by_key(|r| r.seq)?;
    (!owner.addr.is_empty()).then_some(owner.addr)
}

/// Announces this runtime's endpoint. The store rejects nothing here -- a
/// runtime may only ever describe its own row.
pub fn send_announce_endpoint(conn: &DbConnection, addr: &str) -> Result<(), String> {
    conn.reducers
        .announce_endpoint(addr.to_string())
        .map_err(|e| format!("announce_endpoint failed: {e}"))
}

// --- Send side: local edits -> reducers. --------------------------------------
//
// Every one of these can fail, and a failure means the edit did not happen:
// the caller has already applied it locally, so it is the only one that can
// say the shared graph and this window no longer agree. Logging it here and
// returning nothing is what made a whole editing session vanish quietly when
// the store went away, so the answer travels to the caller instead.

pub fn send_create_node(conn: &DbConnection, n: &NodeData) -> Result<(), String> {
    conn.reducers
        .create_node(
            n.id,
            n.type_id.clone(),
            n.display_name.clone(),
            n.x,
            n.y,
            params_json(&n.params),
            n.parent,
        )
        .map_err(|e| format!("create_node failed: {e}"))
}

pub fn send_move_node(conn: &DbConnection, id: u64, x: f32, y: f32) -> Result<(), String> {
    conn.reducers
        .move_node(id, x, y)
        .map_err(|e| format!("move_node failed: {e}"))
}

pub fn send_set_params(
    conn: &DbConnection,
    id: u64,
    params: &[(String, String)],
) -> Result<(), String> {
    conn.reducers
        .set_node_params(id, params_json(params))
        .map_err(|e| format!("set_node_params failed: {e}"))
}

pub fn send_delete_node(conn: &DbConnection, id: u64) -> Result<(), String> {
    conn.reducers
        .delete_node(id)
        .map_err(|e| format!("delete_node failed: {e}"))
}

pub fn send_connect_edge(conn: &DbConnection, e: &EdgeData) -> Result<(), String> {
    conn.reducers
        .connect_edge(
            e.id,
            e.from_node,
            e.from_pin.clone(),
            e.to_node,
            e.to_pin.clone(),
        )
        .map_err(|e| format!("connect_edge failed: {e}"))
}

pub fn send_disconnect_edge(conn: &DbConnection, id: u64) -> Result<(), String> {
    conn.reducers
        .disconnect_edge(id)
        .map_err(|e| format!("disconnect_edge failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restarting host is back within a second, and one that is gone for the
    /// afternoon must not be asked in a tight loop -- so the schedule has to
    /// start short, grow, and stop growing.
    #[test]
    fn the_retry_schedule_grows_and_stops() {
        assert_eq!(retry_delay(1), RETRY_MIN);
        assert_eq!(retry_delay(2), RETRY_MIN * 2);
        assert_eq!(retry_delay(3), RETRY_MIN * 4);
        // Monotonic and capped, including at absurd attempt counts, where the
        // arithmetic must not overflow.
        let mut previous = Duration::ZERO;
        for attempt in 1..1000 {
            let delay = retry_delay(attempt);
            assert!(delay >= previous, "the schedule never shortens");
            assert!(delay <= RETRY_MAX, "and never exceeds the cap");
            previous = delay;
        }
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }

    /// One token file per store: an identity issued by one host means nothing
    /// to another, and the key has to be a usable filename.
    #[test]
    fn the_credential_key_names_the_store() {
        assert_eq!(
            credential_key("http://127.0.0.1:3000", "zeughaus"),
            "zeughaus-127-0-0-1-3000-zeughaus"
        );
        assert_ne!(
            credential_key("http://10.0.0.8:3000", "zeughaus"),
            credential_key("http://127.0.0.1:3000", "zeughaus")
        );
        assert!(!credential_key("http://host/../x", "db").contains('/'));
    }
}
