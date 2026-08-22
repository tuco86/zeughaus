//! Headless graph runtime: the only process that executes a Zeughaus graph.
//!
//! It joins a session's shared store as a runtime, applies every graph change it
//! observes to a [`GraphExecutor`], and publishes the scalar outputs it computes.
//! Editors -- local or remote, native or browser -- edit and display; they never
//! run a node. That is what keeps a node with side effects (a screen capture, an
//! LLM request) firing once per session instead of once per open window.
//!
//! Several runners may join the same session. The store orders them and only the
//! first executes; the others are hot standbys that take over when it leaves,
//! which is why nothing here negotiates ownership -- it is read back out of the
//! store on every batch.

mod runner;

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::Duration;

use zeughaus_core::{NodeId, Value, ZeughausError};
use zeughaus_runtime::DeferredWork;
use zeughaus_sync::Role;

use crate::runner::{AsyncResult, Runner};

/// How long the loop blocks on the event channel before looking around.
///
/// The SDK delivers row changes over a `std::sync::mpsc` channel, which cannot
/// be selected over together with the async-result channel, so completed node
/// work is picked up between batches. This is therefore the worst-case latency
/// for an async result and for an ownership change -- short enough to be
/// imperceptible, long enough that an idle runner is free.
const TICK: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    // A unique id range per process, so a runner that creates ids (none today,
    // but nodes may spawn nodes) can never collide with a live editor's.
    zeughaus_core::NodeId::seed_unique();
    zeughaus_core::EdgeId::seed_unique();

    let (uri, db, token) = resolve_session(parse_join_arg());
    eprintln!("[runner] session {token} -> {uri} / {db}");

    let (conn, sync_rx) = match zeughaus_sync::connect(&uri, &db, Role::Runtime) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!(
                "[runner] cannot connect to {uri} / {db}: {e} (start it with `spacetime start`)"
            );
            return ExitCode::FAILURE;
        }
    };

    // Blocking node work runs here. One worker is enough: it only awaits the
    // blocking pool's join handles, which is what turns a panicking node into a
    // reported error instead of a node left pending forever.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("zeughaus-async")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[runner] cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut runner = Runner::new(conn);
    let (async_tx, async_rx) = std::sync::mpsc::channel::<(NodeId, AsyncResult)>();

    // Ctrl-C is left to the default disposition on purpose: the process dies,
    // its connection closes, and the module drops its `runtime` row -- which is
    // exactly the handover a standby waits for. A signal handler could only
    // repeat that, and would add a dependency to do it.
    loop {
        // Block for the first event, then take whatever else is already queued:
        // one pass per burst of row changes rather than one per row. A
        // subscription applying delivers a whole graph this way.
        let mut events = Vec::new();
        match sync_rx.recv_timeout(TICK) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[runner] store connection closed, exiting");
                return ExitCode::FAILURE;
            }
        }
        while let Ok(event) = sync_rx.try_recv() {
            events.push(event);
        }

        let mut results = Vec::new();
        while let Ok(result) = async_rx.try_recv() {
            results.push(result);
        }

        if events.is_empty() && results.is_empty() {
            continue;
        }

        // Before anything is applied or published: a pass must not run on an
        // ownership this process no longer has.
        runner.refresh_ownership();

        for (node_id, result) in results {
            let deferred = runner.deliver(node_id, result);
            dispatch(&rt, &async_tx, deferred);
        }

        if !events.is_empty() {
            for event in events {
                runner.apply(event);
            }
            let deferred = runner.pass();
            dispatch(&rt, &async_tx, deferred);
        }
    }
}

/// Hands deferred node work to the blocking pool, reporting each outcome back
/// over `tx`.
///
/// The join handle is awaited on a runtime worker rather than here, because the
/// work takes seconds (an LLM request) and the event loop has to keep draining
/// the store meanwhile. A join failure is reported like any other error: the
/// executor holds every downstream node back while a node is pending, so a
/// panicking node that never reported would freeze that whole subtree.
fn dispatch(
    rt: &tokio::runtime::Runtime,
    tx: &Sender<(NodeId, AsyncResult)>,
    deferred: DeferredWork,
) {
    for (node_id, work) in deferred {
        let tx = tx.clone();
        rt.spawn(async move {
            let outputs: Result<HashMap<String, Value>, ZeughausError> =
                tokio::task::spawn_blocking(move || work.run())
                    .await
                    .unwrap_or_else(|e| {
                        Err(ZeughausError::ExecutionFailed(format!(
                            "background task failed: {e}"
                        )))
                    });
            let _ = tx.send((node_id, outputs.map_err(|e| e.to_string())));
        });
    }
}

/// Parses `join <session>` from the CLI args, mirroring the editor's argument
/// shape. `None` runs the default local session.
fn parse_join_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "join" {
            return args.next();
        }
    }
    None
}

/// Turns a session token into `(uri, database, token)`. Without one, the runner
/// serves the default session on the local host, and reports the token a remote
/// editor would use to reach it.
fn resolve_session(session: Option<String>) -> (String, String, String) {
    match session {
        Some(token) => {
            let (uri, db) = zeughaus_sync::parse_token(&token);
            (uri, db, token)
        }
        None => {
            let token = format!(
                "{}:{}/{}",
                zeughaus_sync::lan_ip(),
                zeughaus_sync::DEFAULT_PORT,
                zeughaus_sync::DEFAULT_SESSION
            );
            (
                format!("http://127.0.0.1:{}", zeughaus_sync::DEFAULT_PORT),
                zeughaus_sync::DEFAULT_SESSION.to_string(),
                token,
            )
        }
    }
}
