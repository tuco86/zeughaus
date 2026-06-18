//! SpacetimeDB server module: the central collaborative store for graph state.
//!
//! Tables mirror `zeughaus_core::GraphDocument` (node + edge). Reducers are the
//! only way to mutate state; clients call them and observe the resulting table
//! changes via subscriptions. Node parameters are stored as a JSON string to
//! avoid nested tables in this first iteration.
//!
//! Built for the wasm32 module target with `spacetime build` (this crate is
//! excluded from the native workspace build).

use spacetimedb::{ReducerContext, Table, reducer, table};

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
