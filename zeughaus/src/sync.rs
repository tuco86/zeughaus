//! Minimal SpacetimeDB connect/subscribe layer (Phase 2).
//!
//! Connects to a running `zeughaus` module, subscribes to the `node` and `edge`
//! tables, and logs row changes. It does NOT yet apply remote changes to the
//! editor state nor push local edits via reducers -- that is Phase 4 (full
//! sync). This is the foundation: proving the client can connect, subscribe and
//! observe the collaborative store.
//!
//! Opt-in: only runs when `ZEUGHAUS_STDB_URI` is set, so the default editor is
//! unchanged and never blocks on an absent server.

use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};

use crate::module_bindings::{DbConnection, EdgeTableAccess, NodeTableAccess};

const DEFAULT_MODULE: &str = "zeughaus";

/// Connects, registers row-change logging, subscribes to node+edge, and spawns
/// the background message loop. The returned connection must be kept alive for
/// the loop to keep running.
fn connect(uri: &str, module: &str) -> Result<DbConnection, String> {
    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(module)
        .on_connect(|_ctx, identity, _token| {
            eprintln!("[stdb] connected as {identity:?}");
        })
        .on_connect_error(|_ctx, err| {
            eprintln!("[stdb] connect error: {err}");
        })
        .on_disconnect(|_ctx, err| match err {
            Some(e) => eprintln!("[stdb] disconnected: {e}"),
            None => eprintln!("[stdb] disconnected"),
        })
        .build()
        .map_err(|e| format!("build failed: {e}"))?;

    // Observe the collaborative store. Phase 4 will apply these to editor state;
    // for now they are logged so the subscription is demonstrably live.
    conn.db.node().on_insert(|_ctx, n| {
        eprintln!("[stdb] node + {} {} @({}, {})", n.id, n.type_id, n.x, n.y);
    });
    conn.db.node().on_update(|_ctx, _old, n| {
        eprintln!("[stdb] node ~ {} @({}, {})", n.id, n.x, n.y);
    });
    conn.db.node().on_delete(|_ctx, n| {
        eprintln!("[stdb] node - {}", n.id);
    });
    conn.db.edge().on_insert(|_ctx, e| {
        eprintln!("[stdb] edge + {} {}->{}", e.id, e.from_node, e.to_node);
    });
    conn.db.edge().on_delete(|_ctx, e| {
        eprintln!("[stdb] edge - {}", e.id);
    });

    conn.subscription_builder()
        .on_applied(|_ctx| eprintln!("[stdb] subscription applied"))
        .on_error(|_ctx, err| eprintln!("[stdb] subscription error: {err}"))
        .subscribe(["SELECT * FROM node", "SELECT * FROM edge"]);

    // Advance the connection on a background thread (native). The handle is
    // detached; the thread runs until the connection ends.
    conn.run_threaded();

    Ok(conn)
}

/// Opt-in entry point: connects only when `ZEUGHAUS_STDB_URI` is set. Returns
/// the live connection (to be kept alive), or `None` when sync is disabled or
/// the connection could not be established.
pub fn maybe_connect() -> Option<DbConnection> {
    let uri = std::env::var("ZEUGHAUS_STDB_URI").ok()?;
    let module =
        std::env::var("ZEUGHAUS_STDB_MODULE").unwrap_or_else(|_| DEFAULT_MODULE.to_string());
    match connect(&uri, &module) {
        Ok(conn) => {
            eprintln!("[stdb] sync enabled: {uri} / {module}");
            Some(conn)
        }
        Err(e) => {
            eprintln!("[stdb] sync disabled: {e}");
            None
        }
    }
}
