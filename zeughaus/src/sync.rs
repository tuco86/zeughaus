//! SpacetimeDB sync layer: bidirectional graph synchronization.
//!
//! - Receive: subscribes to the `node` and `edge` tables; row changes are
//!   converted to [`SyncEvent`]s and pushed onto a channel the editor drains on
//!   a timer (the SDK callbacks run on a background thread, so we hand events to
//!   the iced main loop rather than touch editor state directly).
//! - Send: helper functions call the module's reducers when the user edits the
//!   graph locally.
//!
//! Conflict model: fine-grained reducers, last-writer-wins per row. Node/edge
//! ids are made process-unique at startup (see `NodeId::seed_unique`) so two
//! editors never assign colliding ids. Applying a remote change is guarded so it
//! does not echo back as a reducer call.
//!
//! Always-on: the editor connects to SpacetimeDB on startup with no local-only
//! fallback. A missing server is a fatal startup error, not a degraded mode.

use std::sync::mpsc::{Receiver, Sender};

use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};
use zeughaus_core::{EdgeData, NodeData};

use crate::module_bindings::{
    DbConnection, Edge, EdgeTableAccess, Node, NodeOutputTableAccess, NodeTableAccess,
    RuntimeTableAccess, clear_node_outputs, connect_edge, create_node, delete_node,
    disconnect_edge, join_runtime, move_node, publish_output, set_node_params,
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
    /// A published output value changed or disappeared. Also payload-free for
    /// the same reason -- a viewer rebuilds a node's whole output set from the
    /// cache, because a per-pin event cannot say which pins are still absent.
    OutputsChanged,
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

/// Connects, wires row-change callbacks into a channel, subscribes to node+edge,
/// and spawns the background message loop. Returns the live connection (kept
/// alive by the caller) and the receiving end of the event channel. SpacetimeDB
/// is required; the error is fatal to the caller (no local-only fallback).
pub fn connect(uri: &str, module: &str) -> Result<(DbConnection, Receiver<SyncEvent>), String> {
    let (tx, rx) = std::sync::mpsc::channel();

    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(module)
        .on_connect(|ctx, identity, _token| {
            eprintln!("[stdb] connected as {identity:?}");
            // Registering is opt-in, so only editors compete for ownership --
            // a `spacetime sql` connection is a client too, and must not become
            // the runtime everyone else waits for.
            if let Err(e) = ctx.reducers.join_runtime() {
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
    conn.db.node_output().on_insert(move |_ctx, _o| send(&t, SyncEvent::OutputsChanged));
    let t = tx.clone();
    conn.db
        .node_output()
        .on_update(move |_ctx, _old, _new| send(&t, SyncEvent::OutputsChanged));
    let t = tx.clone();
    conn.db.node_output().on_delete(move |_ctx, _o| send(&t, SyncEvent::OutputsChanged));

    conn.subscription_builder()
        .on_applied(|_ctx| eprintln!("[stdb] subscription applied"))
        .on_error(|_ctx, err| eprintln!("[stdb] subscription error: {err}"))
        .subscribe([
            "SELECT * FROM node",
            "SELECT * FROM edge",
            "SELECT * FROM runtime",
            "SELECT * FROM node_output",
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
/// the graph, everyone else displays what it publishes.
///
/// Read from the client cache rather than remembered, so a runtime leaving
/// hands ownership over without any handshake. While the cache is still empty
/// (before the first subscription applies) nobody owns anything, which keeps a
/// starting editor from executing a graph it has not seen yet.
pub fn is_owner(conn: &DbConnection) -> bool {
    conn.db
        .runtime()
        .iter()
        .min_by_key(|r| r.seq)
        .is_some_and(|r| r.identity == conn.identity())
}

/// This client's position in the runtime order, for showing the user which
/// window they are looking at. `0` is the owner.
pub fn runtime_index(conn: &DbConnection) -> Option<usize> {
    let mut seqs: Vec<(u64, bool)> = conn
        .db
        .runtime()
        .iter()
        .map(|r| (r.seq, r.identity == conn.identity()))
        .collect();
    seqs.sort_by_key(|(seq, _)| *seq);
    seqs.iter().position(|(_, is_me)| *is_me)
}

/// Every published output as `(node_id, pin, type tag, text)`.
///
/// A viewer rebuilds a node's whole output set from this, because absence is
/// meaningful: a pin with no row produced no value, which is what the editor
/// draws dimmed.
pub fn published_outputs(conn: &DbConnection) -> Vec<(u64, String, String, String)> {
    conn.db
        .node_output()
        .iter()
        .map(|o| (o.node_id, o.pin, o.ty, o.value))
        .collect()
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

/// Publishes one scalar output. The module drops it unless this client owns
/// execution, so a viewer calling this is harmless.
pub fn send_publish_output(conn: &DbConnection, node_id: u64, pin: &str, ty: &str, value: &str) {
    if let Err(e) = conn
        .reducers
        .publish_output(node_id, pin.to_string(), ty.to_string(), value.to_string())
    {
        eprintln!("[stdb] publish_output failed: {e}");
    }
}

pub fn send_clear_node_outputs(conn: &DbConnection, node_id: u64) {
    if let Err(e) = conn.reducers.clear_node_outputs(node_id) {
        eprintln!("[stdb] clear_node_outputs failed: {e}");
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
