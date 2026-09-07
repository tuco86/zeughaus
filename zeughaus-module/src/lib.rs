//! SpacetimeDB server module: the central collaborative store for graph state.
//!
//! Tables mirror `zeughaus_core::GraphDocument` (node + edge). Reducers are the
//! only way to mutate state; clients call them and observe the resulting table
//! changes via subscriptions. Node parameters are stored as a JSON string to
//! avoid nested tables in this first iteration.
//!
//! Beyond the graph itself, the store decides WHO runs it and WHERE that
//! runtime is reachable: every runner registers in `runtime`, the one with the
//! lowest `seq` owns execution, and its row carries the pinned URL editors
//! dial. What a pass produces does not travel through here at all -- values,
//! edge traffic, frames and trigger presses go over weida, straight between
//! the process that computed them and the windows that draw them.
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
    /// The pinned root URL editors reach this runtime at
    /// (`weida://sha256:<fp>@host:port/`), or empty while it serves nothing.
    /// Runtime values do not travel through this store -- the store's job is to
    /// say WHERE they travel, and the fingerprint in the URL is what makes that
    /// address trustworthy without distributing a certificate.
    ///
    /// Defaulted so gaining the endpoint is an automatic migration: an existing
    /// session must not have to be deleted for it.
    #[default("")]
    pub addr: String,
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
    /// The container node this node lives inside, `0` for the root graph.
    ///
    /// Indexed because deleting a container walks its children, and defaulted
    /// so gaining subgraphs is an automatic migration: every node in an
    /// existing session belongs to the root graph.
    #[default(0)]
    #[index(btree)]
    pub parent: u64,
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
    parent: u64,
) {
    ctx.db.node().insert(Node {
        id,
        type_id,
        display_name,
        x,
        y,
        params,
        parent,
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

/// Deletes a node, everything inside it, and every edge that touched any of
/// them.
///
/// Recursive because a container node IS its contents: deleting the container
/// row alone would leave its children in the store, parented to a node that no
/// longer exists and reachable from no graph.
#[reducer]
pub fn delete_node(ctx: &ReducerContext, id: u64) {
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        ctx.db.node().id().delete(current);
        for child in ctx.db.node().parent().filter(current) {
            stack.push(child.id);
        }
        // Remove dangling edges that referenced this node.
        let dangling: Vec<u64> = ctx
            .db
            .edge()
            .iter()
            .filter(|e| e.from_node == current || e.to_node == current)
            .map(|e| e.id)
            .collect();
        for eid in dangling {
            ctx.db.edge().id().delete(eid);
        }
    }
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
        addr: String::new(),
    });
}

/// Announces where this runtime is reachable, and thereby whom to trust: the
/// URL pins the runtime's public-key fingerprint.
///
/// Separate from joining because the address is only known once the listener is
/// bound, and a runtime is useful before that happens. Only the caller's own
/// row is touched, so no runtime can redirect an editor somewhere else.
#[reducer]
pub fn announce_endpoint(ctx: &ReducerContext, addr: String) {
    if let Some(mut row) = ctx.db.runtime().identity().find(ctx.sender()) {
        row.addr = addr;
        ctx.db.runtime().identity().update(row);
    }
}

/// Drops a runtime when its editor disconnects, which is what hands ownership
/// to the next one. Nothing else has to be cleaned up: values live in the
/// runner that computed them, so they leave with it.
#[reducer(client_disconnected)]
pub fn on_client_disconnected(ctx: &ReducerContext) {
    ctx.db.runtime().identity().delete(ctx.sender());
}

