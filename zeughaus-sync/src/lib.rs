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

use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};
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

/// Connects, wires row-change callbacks into a channel, subscribes to the shared
/// tables, and spawns the background message loop. Returns the live connection
/// (kept alive by the caller) and the receiving end of the event channel.
/// SpacetimeDB is required; the error is fatal to the caller (no local-only
/// fallback).
pub fn connect(
    uri: &str,
    module: &str,
    role: Role,
) -> Result<(DbConnection, Receiver<SyncEvent>), String> {
    let (tx, rx) = std::sync::mpsc::channel();

    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(module)
        .on_connect(move |ctx, identity, _token| {
            eprintln!("[stdb] connected as {identity:?} ({role:?})");
            // Registering happens here rather than after `connect` returns:
            // a reducer call needs the connection to be established, and this
            // callback is the first point where it is.
            if role == Role::Runtime
                && let Err(e) = ctx.reducers.join_runtime()
            {
                eprintln!("[stdb] join_runtime failed: {e}");
            }
        })
        .on_connect_error(|_ctx, err| eprintln!("[stdb] connect error: {err}"))
        .on_disconnect(|_ctx, err| match err {
            Some(e) => eprintln!("[stdb] disconnected: {e}"),
            None => eprintln!("[stdb] disconnected"),
        })
        .build()
        .map_err(|e| format!("build failed: {e}"))?;

    // Forward table changes onto the channel. Self-originated changes are echoed
    // back here too; the editor applies them idempotently.
    let t = tx.clone();
    conn.db.node().on_insert(move |_ctx, n| send(&t, SyncEvent::NodeUpsert(to_node_data(n))));
    let t = tx.clone();
    conn.db.node().on_update(move |_ctx, _old, n| send(&t, SyncEvent::NodeUpsert(to_node_data(n))));
    let t = tx.clone();
    conn.db.node().on_delete(move |_ctx, n| send(&t, SyncEvent::NodeRemove(n.id)));
    let t = tx.clone();
    conn.db.edge().on_insert(move |_ctx, e| send(&t, SyncEvent::EdgeInsert(to_edge_data(e))));
    let t = tx.clone();
    conn.db.edge().on_delete(move |_ctx, e| send(&t, SyncEvent::EdgeRemove(e.id)));
    let t = tx.clone();
    conn.db.runtime().on_insert(move |_ctx, _r| send(&t, SyncEvent::RuntimesChanged));
    let t = tx.clone();
    conn.db.runtime().on_delete(move |_ctx, _r| send(&t, SyncEvent::RuntimesChanged));

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
    Ok((conn, rx))
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
        None => (format!("http://127.0.0.1:{DEFAULT_PORT}"), token.to_string()),
    }
}

fn params_json(params: &[(String, String)]) -> String {
    serde_json::to_string(params).unwrap_or_else(|_| "[]".to_string())
}

/// Whether this client owns execution: the runtime with the lowest `seq` runs
/// the graph, a second one is a standby. Only meaningful for a
/// [`Role::Runtime`] client.
///
/// Read from the client cache rather than remembered, so a runtime leaving
/// hands ownership over without any handshake. While the cache is still empty
/// (before the first subscription applies) nobody owns anything, which keeps a
/// starting runner from executing a graph it has not seen yet.
pub fn is_owner(conn: &DbConnection) -> bool {
    conn.db
        .runtime()
        .iter()
        .min_by_key(|r| r.seq)
        .is_some_and(|r| r.identity == conn.identity())
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
pub fn send_announce_endpoint(conn: &DbConnection, addr: &str) {
    if let Err(e) = conn.reducers.announce_endpoint(addr.to_string()) {
        eprintln!("[stdb] announce_endpoint failed: {e}");
    }
}

// --- Send side: local edits -> reducers. Errors are logged, not fatal. -------

pub fn send_create_node(conn: &DbConnection, n: &NodeData) {
    if let Err(e) = conn.reducers.create_node(
        n.id,
        n.type_id.clone(),
        n.display_name.clone(),
        n.x,
        n.y,
        params_json(&n.params),
    ) {
        eprintln!("[stdb] create_node failed: {e}");
    }
}

pub fn send_move_node(conn: &DbConnection, id: u64, x: f32, y: f32) {
    if let Err(e) = conn.reducers.move_node(id, x, y) {
        eprintln!("[stdb] move_node failed: {e}");
    }
}

pub fn send_set_params(conn: &DbConnection, id: u64, params: &[(String, String)]) {
    if let Err(e) = conn.reducers.set_node_params(id, params_json(params)) {
        eprintln!("[stdb] set_node_params failed: {e}");
    }
}

pub fn send_delete_node(conn: &DbConnection, id: u64) {
    if let Err(e) = conn.reducers.delete_node(id) {
        eprintln!("[stdb] delete_node failed: {e}");
    }
}

pub fn send_connect_edge(conn: &DbConnection, e: &EdgeData) {
    if let Err(err) = conn.reducers.connect_edge(
        e.id,
        e.from_node,
        e.from_pin.clone(),
        e.to_node,
        e.to_pin.clone(),
    ) {
        eprintln!("[stdb] connect_edge failed: {err}");
    }
}

pub fn send_disconnect_edge(conn: &DbConnection, id: u64) {
    if let Err(e) = conn.reducers.disconnect_edge(id) {
        eprintln!("[stdb] disconnect_edge failed: {e}");
    }
}
