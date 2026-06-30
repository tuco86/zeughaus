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
//! Opt-in: only runs when `ZEUGHAUS_STDB_URI` is set, so the default editor is
//! unchanged and never blocks on an absent server.

use std::sync::mpsc::{Receiver, Sender};

use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};
use zeughaus_core::{EdgeData, NodeData};

use crate::module_bindings::{
    connect_edge, create_node, delete_node, disconnect_edge, move_node, set_node_params,
    DbConnection, Edge, EdgeTableAccess, Node, NodeTableAccess,
};

const DEFAULT_URI: &str = "http://127.0.0.1:3000";

/// A change observed in the shared store, to be applied to the editor.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    NodeUpsert(NodeData),
    NodeRemove(u64),
    EdgeInsert(EdgeData),
    EdgeRemove(u64),
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
/// alive by the caller) and the receiving end of the event channel.
fn connect(uri: &str, module: &str) -> Result<(DbConnection, Receiver<SyncEvent>), String> {
    let (tx, rx) = std::sync::mpsc::channel();

    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(module)
        .on_connect(|_ctx, identity, _token| eprintln!("[stdb] connected as {identity:?}"))
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

    conn.subscription_builder()
        .on_applied(|_ctx| eprintln!("[stdb] subscription applied"))
        .on_error(|_ctx, err| eprintln!("[stdb] subscription error: {err}"))
        .subscribe(["SELECT * FROM node", "SELECT * FROM edge"]);

    conn.run_threaded();
    Ok((conn, rx))
}

fn send(tx: &Sender<SyncEvent>, ev: SyncEvent) {
    // The receiver lives for the app's lifetime; a send error just means the
    // editor is shutting down, which we can ignore.
    let _ = tx.send(ev);
}

/// Joins a collaboration session: connects to the SpacetimeDB database named by
/// `session_id` and subscribes to it. The server URI defaults to a local
/// instance and can be overridden with `ZEUGHAUS_STDB_URI`. Returns the live
/// connection plus the event receiver, or `None` if the connection failed.
pub fn connect_session(session_id: &str) -> Option<(DbConnection, Receiver<SyncEvent>)> {
    let uri = std::env::var("ZEUGHAUS_STDB_URI").unwrap_or_else(|_| DEFAULT_URI.to_string());
    match connect(&uri, session_id) {
        Ok(pair) => {
            eprintln!("[stdb] joined session '{session_id}' at {uri}");
            Some(pair)
        }
        Err(e) => {
            eprintln!("[stdb] could not join '{session_id}': {e}");
            None
        }
    }
}

fn params_json(params: &[(String, String)]) -> String {
    serde_json::to_string(params).unwrap_or_else(|_| "[]".to_string())
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
