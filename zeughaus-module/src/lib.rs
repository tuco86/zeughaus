//! SpacetimeDB server module: the central collaborative store for graph state.
//!
//! Three tables, all public. `node` and `edge` are the graph every editor and
//! runner subscribes to -- one row per node and per edge, with a node's
//! parameters as a JSON string rather than a table of their own, so a
//! parameter set is written and read in one row. `runtime` is presence:
//! reducers are the only way to mutate any of it, and clients observe the
//! resulting row changes through their subscriptions.
//!
//! Beyond the graph itself, the store decides WHO runs it and WHERE that
//! runtime is reachable: every runner registers in `runtime`, its row carries
//! the pinned URL editors dial, and every top-level graph names the runner
//! that executes it. What a pass produces does not travel through here at
//! all -- values, edge traffic, frames and trigger presses go over weida,
//! straight between the process that computed them and the windows that draw
//! them.
//!
//! Built for the wasm32 module target with `spacetime build` (this crate is
//! excluded from the native workspace build).

use spacetimedb::{ConnectionId, Identity, ReducerContext, Table, log, reducer, table};

/// One connected runner. `seq` is monotonic, so "lowest seq" is a stable,
/// server-decided answer to "who executes" that needs no election protocol:
/// whoever has been here longest owns it, and when they leave the next one
/// inherits it.
///
/// Keyed by CONNECTION, not by identity: every process on one machine loads the
/// same saved token and is therefore the same identity, so an identity key
/// would let a closing editor delete the running runner's row, and would make
/// the hot standby impossible because the second runner's `join_runtime` would
/// find the first one's row. The identity stays as a plain column: it says who
/// owns the process, which is what a permission check reads.
#[table(accessor = runtime, name = "runtime", public)]
pub struct Runtime {
    #[primary_key]
    pub connection_id: ConnectionId,
    pub identity: Identity,
    #[unique]
    #[auto_inc]
    pub seq: u64,
    /// The pinned root URL editors reach this runtime at
    /// (`weida://sha256:<fp>@host:port/`), or empty while it serves nothing.
    /// Runtime values do not travel through this store -- the store's job is to
    /// say WHERE they travel, and the fingerprint in the URL is what makes that
    /// address trustworthy without distributing a certificate.
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
    /// The runner that executes this graph (`sha256:<hex>` fingerprint from
    /// its endpoint URL). Set on top-level graphs only; empty everywhere else.
    #[default("")]
    pub runner: String,
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
    runner: String,
) {
    ctx.db.node().insert(Node {
        id,
        type_id,
        display_name,
        x,
        y,
        params,
        parent,
        runner,
    });
}

/// Moves every root-level node that belongs to no runner into a new top-level
/// graph owned by `runner`.
///
/// Sessions from before graphs had owners keep their nodes at the root; a node
/// there is executed by nobody, so the first runner to see them adopts them
/// as one graph. Does nothing when there is nothing to adopt, which makes a
/// second call (another runner racing the first) harmless.
#[reducer]
pub fn adopt_root_nodes(ctx: &ReducerContext, graph_id: u64, runner: String) {
    let orphans: Vec<Node> = ctx
        .db
        .node()
        .parent()
        .filter(0u64)
        .filter(|n| n.runner.is_empty())
        .collect();
    if orphans.is_empty() {
        return;
    }
    ctx.db.node().insert(Node {
        id: graph_id,
        type_id: "graph.sub".to_string(),
        display_name: "Graph".to_string(),
        x: 0.0,
        y: 0.0,
        params: "[]".to_string(),
        parent: 0,
        runner,
    });
    let n = orphans.len();
    for mut node in orphans {
        if node.id == graph_id {
            continue;
        }
        node.parent = graph_id;
        ctx.db.node().id().update(node);
    }
    log::info!("adopt_root_nodes {n} into {graph_id} by {}", ctx.sender());
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
pub fn rename_node(ctx: &ReducerContext, id: u64, display_name: String) {
    if let Some(mut n) = ctx.db.node().id().find(id) {
        n.display_name = display_name;
        ctx.db.node().id().update(n);
    }
}

/// Deletes a node, everything inside it, and every edge that touched any of
/// them.
///
/// Recursive because a container node IS its contents: deleting the container
/// row alone would leave its children in the store, parented to a node that no
/// longer exists and reachable from no graph.
///
/// Every call is logged with its caller. This and `disconnect_edge` are the
/// only reducers that destroy graph state, and a row that vanished with nobody
/// able to say which client asked for it is a bug nobody can chase --
/// `spacetime logs` otherwise records only the calls that FAILED.
#[reducer]
pub fn delete_node(ctx: &ReducerContext, id: u64) {
    log::info!("delete_node {id} by {}", ctx.sender());
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
    log::info!("disconnect_edge {id} by {}", ctx.sender());
    ctx.db.edge().id().delete(id);
}

/// Registers the calling CONNECTION as a runtime that wants to run the graph.
/// `seq` is assigned by the store, so ownership never depends on client clocks
/// or on who shouts first.
///
/// Explicitly called by runners rather than hooked to `client_connected`: every
/// `spacetime sql` or `spacetime call` is a client too, and one of those holding
/// the lowest seq would make a CLI invocation the owner of execution -- leaving
/// every editor waiting for a runtime that is not there.
///
/// Two runners on one machine are two connections with one identity, so the
/// second one joins as a standby instead of finding the first one's row.
#[reducer]
pub fn join_runtime(ctx: &ReducerContext) {
    let Some(conn) = ctx.connection_id() else {
        log::warn!("join_runtime from {} without a connection", ctx.sender());
        return;
    };
    if ctx.db.runtime().connection_id().find(conn).is_some() {
        return;
    }
    ctx.db.runtime().insert(Runtime {
        connection_id: conn,
        identity: ctx.sender(),
        seq: 0, // auto_inc
        addr: String::new(),
    });
}

/// Announces where this runtime is reachable, and thereby whom to trust: the
/// URL pins the runtime's public-key fingerprint.
///
/// Separate from joining because the address is only known once the listener is
/// bound, and a runtime is useful before that happens. Only the calling
/// connection's own row is touched, so no runtime can redirect an editor
/// somewhere else -- not even another process of the same user.
#[reducer]
pub fn announce_endpoint(ctx: &ReducerContext, addr: String) {
    let Some(conn) = ctx.connection_id() else {
        return;
    };
    if let Some(mut row) = ctx.db.runtime().connection_id().find(conn) {
        row.addr = addr;
        ctx.db.runtime().connection_id().update(row);
    }
}

/// Drops a runtime when its connection goes away, which is what hands ownership
/// to the next one. Nothing else has to be cleaned up: values live in the
/// runner that computed them, so they leave with it.
///
/// By connection, so an editor closing on the same machine -- same identity,
/// different connection -- leaves the running runner registered.
#[reducer(client_disconnected)]
pub fn on_client_disconnected(ctx: &ReducerContext) {
    if let Some(conn) = ctx.connection_id() {
        ctx.db.runtime().connection_id().delete(conn);
    }
}
