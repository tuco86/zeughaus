//! SpacetimeDB server module: the central collaborative store for graph state.
//!
//! Tables mirror `zeughaus_core::GraphDocument` (node + edge). Reducers are the
//! only way to mutate state; clients call them and observe the resulting table
//! changes via subscriptions. Node parameters are stored as a JSON string to
//! avoid nested tables in this first iteration.
//!
//! Beyond the graph itself, the store decides WHO runs it. Every connected
//! editor registers in `runtime`; the one with the lowest `seq` owns execution
//! and publishes its scalar results into `node_output`, which the others
//! display. Without that, each window ran the graph for itself -- two windows
//! meant two screenshots from one capture node, each seeing its own.
//!
//! Built for the wasm32 module target with `spacetime build` (this crate is
//! excluded from the native workspace build).

use spacetimedb::{Identity, ReducerContext, Table, reducer, table};

/// A connected editor. `seq` is monotonic, so "lowest seq" is a stable,
/// server-decided answer to "who executes" that needs no election protocol:
/// whoever has been here longest owns it, and when they leave the next one
/// inherits it.
#[table(accessor = runtime, name = "runtime", public)]
pub struct Runtime {
    #[primary_key]
    pub identity: Identity,
    #[unique]
    #[auto_inc]
    pub seq: u64,
}

/// One output pin's last published value, owned by the executing runtime.
///
/// Only scalars travel: the key is `"<node_id>:<pin>"` because a table takes a
/// single primary key, and the value is text tagged with its type so a viewer
/// can rebuild the value without guessing. Frames and other opaque payloads are
/// deliberately absent -- a 4K frame is 33 MB and a state store is the wrong
/// pipe for it.
#[table(accessor = node_output, name = "node_output", public)]
pub struct NodeOutput {
    #[primary_key]
    pub key: String,
    pub node_id: u64,
    pub pin: String,
    pub ty: String,
    pub value: String,
}

/// A graph node: position, type, display name, and serialized parameters.
#[table(accessor = node, name = "node", public)]
pub struct Node {
    #[primary_key]
    pub id: u64,
    pub type_id: String,
    pub display_name: String,
    pub x: f32,
    pub y: f32,
    /// JSON-encoded parameter list (name -> value), mirroring NodeData::params.
    pub params: String,
}

/// A pending "fire this node once" request, raised by an editor and consumed by
/// the executing runtime.
///
/// A counter rather than a queue: the editor bumps it, the runtime notices the
/// change and fires once. A press cannot be lost by a reconnect (the row
/// survives) and cannot be double-fired by a re-subscription (the count the
/// runtime already handled is the count it compares against).
#[table(accessor = node_trigger, name = "node_trigger", public)]
pub struct NodeTrigger {
    #[primary_key]
    pub node_id: u64,
    pub count: u64,
}

/// A directed edge between two node pins.
#[table(accessor = edge, name = "edge", public)]
pub struct Edge {
    #[primary_key]
    pub id: u64,
    pub from_node: u64,
    pub from_pin: String,
    pub to_node: u64,
    pub to_pin: String,
}

#[reducer]
pub fn create_node(
    ctx: &ReducerContext,
    id: u64,
    type_id: String,
    display_name: String,
    x: f32,
    y: f32,
    params: String,
) {
    ctx.db.node().insert(Node {
        id,
        type_id,
        display_name,
        x,
        y,
        params,
    });
}

#[reducer]
pub fn move_node(ctx: &ReducerContext, id: u64, x: f32, y: f32) {
    if let Some(mut n) = ctx.db.node().id().find(id) {
        n.x = x;
        n.y = y;
        ctx.db.node().id().update(n);
    }
}

#[reducer]
pub fn set_node_params(ctx: &ReducerContext, id: u64, params: String) {
    if let Some(mut n) = ctx.db.node().id().find(id) {
        n.params = params;
        ctx.db.node().id().update(n);
    }
}

#[reducer]
pub fn delete_node(ctx: &ReducerContext, id: u64) {
    ctx.db.node().id().delete(id);
    // Remove dangling edges that referenced this node.
    let dangling: Vec<u64> = ctx
        .db
        .edge()
        .iter()
        .filter(|e| e.from_node == id || e.to_node == id)
        .map(|e| e.id)
        .collect();
    for eid in dangling {
        ctx.db.edge().id().delete(eid);
    }
    // Published outputs die with their node, exactly like its edges: a value
    // whose producer is gone would otherwise sit in the store forever and be
    // adopted by every viewer that joins later.
    let orphaned: Vec<String> = ctx
        .db
        .node_output()
        .iter()
        .filter(|o| o.node_id == id)
        .map(|o| o.key)
        .collect();
    for key in orphaned {
        ctx.db.node_output().key().delete(&key);
    }
    // The press counter goes with it. Node ids are never reused, so a surviving
    // row could only be read as a press for a node that no longer exists.
    ctx.db.node_trigger().node_id().delete(id);
}

#[reducer]
pub fn connect_edge(
    ctx: &ReducerContext,
    id: u64,
    from_node: u64,
    from_pin: String,
    to_node: u64,
    to_pin: String,
) {
    ctx.db.edge().insert(Edge {
        id,
        from_node,
        from_pin,
        to_node,
        to_pin,
    });
}

#[reducer]
pub fn disconnect_edge(ctx: &ReducerContext, id: u64) {
    ctx.db.edge().id().delete(id);
}

/// Bulk replace: clears all state and inserts the given graph. Used for the
/// load-from-document flow.
#[reducer]
pub fn replace_graph(ctx: &ReducerContext, nodes: Vec<Node>, edges: Vec<Edge>) {
    let node_ids: Vec<u64> = ctx.db.node().iter().map(|n| n.id).collect();
    for id in node_ids {
        ctx.db.node().id().delete(id);
    }
    let edge_ids: Vec<u64> = ctx.db.edge().iter().map(|e| e.id).collect();
    for id in edge_ids {
        ctx.db.edge().id().delete(id);
    }
    for n in nodes {
        ctx.db.node().insert(n);
    }
    for e in edges {
        ctx.db.edge().insert(e);
    }
}

/// Registers the caller as a runtime that wants to run the graph. `seq` is
/// assigned by the store, so ownership never depends on client clocks or on who
/// shouts first.
///
/// Explicitly called by editors rather than hooked to `client_connected`: every
/// `spacetime sql` or `spacetime call` is a client too, and one of those holding
/// the lowest seq would make a CLI invocation the owner of execution -- leaving
/// every editor waiting for a runtime that is not there.
#[reducer]
pub fn join_runtime(ctx: &ReducerContext) {
    if ctx.db.runtime().identity().find(ctx.sender()).is_some() {
        return;
    }
    ctx.db.runtime().insert(Runtime {
        identity: ctx.sender(),
        seq: 0, // auto_inc
    });
}

/// Asks the executing runtime to fire a node once. Callable by any editor: it
/// is a request, not a result, and the runtime decides what to do with it.
#[reducer]
pub fn trigger_node(ctx: &ReducerContext, node_id: u64) {
    let count = ctx
        .db
        .node_trigger()
        .node_id()
        .find(node_id)
        .map_or(1, |t| t.count + 1);
    let row = NodeTrigger { node_id, count };
    if count == 1 {
        ctx.db.node_trigger().insert(row);
    } else {
        ctx.db.node_trigger().node_id().update(row);
    }
}

/// Drops a runtime when its editor disconnects, which is what hands ownership
/// to the next one. Published outputs stay: they are the last known values of
/// the graph, and the new owner overwrites them as it runs.
#[reducer(client_disconnected)]
pub fn on_client_disconnected(ctx: &ReducerContext) {
    ctx.db.runtime().identity().delete(ctx.sender());
}

/// Publishes one output pin's value. Ignored unless the caller is the owning
/// runtime, so a viewer cannot overwrite what it is only supposed to display.
#[reducer]
pub fn publish_output(
    ctx: &ReducerContext,
    node_id: u64,
    pin: String,
    ty: String,
    value: String,
) {
    if !is_owner(ctx, ctx.sender()) {
        return;
    }
    let key = output_key(node_id, &pin);
    let row = NodeOutput {
        key: key.clone(),
        node_id,
        pin,
        ty,
        value,
    };
    if ctx.db.node_output().key().find(&key).is_some() {
        ctx.db.node_output().key().update(row);
    } else {
        ctx.db.node_output().insert(row);
    }
}

/// Drops every published output of a node. The owner calls this when a node
/// stops producing a value, so a stale number cannot outlive its source.
#[reducer]
pub fn clear_node_outputs(ctx: &ReducerContext, node_id: u64) {
    if !is_owner(ctx, ctx.sender()) {
        return;
    }
    let keys: Vec<String> = ctx
        .db
        .node_output()
        .iter()
        .filter(|o| o.node_id == node_id)
        .map(|o| o.key)
        .collect();
    for key in keys {
        ctx.db.node_output().key().delete(&key);
    }
}

/// The owning runtime is the one with the lowest `seq`.
fn is_owner(ctx: &ReducerContext, who: Identity) -> bool {
    ctx.db
        .runtime()
        .iter()
        .min_by_key(|r| r.seq)
        .is_some_and(|r| r.identity == who)
}

fn output_key(node_id: u64, pin: &str) -> String {
    format!("{node_id}:{pin}")
}
