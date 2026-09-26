//! Headless graph runtime: the only process that executes a Zeughaus graph.
//!
//! It joins a session's shared store as a runtime, applies every graph change it
//! observes to a [`GraphExecutor`], and publishes the scalar outputs it computes.
//! Editors -- local or remote, native or browser -- edit and display; they never
//! run a node. That is what keeps a node with side effects (a screen capture, an
//! LLM request) firing once per session instead of once per open window.
//!
//! Several runners may join the same session. Every top-level graph names the
//! runner that executes it, and each runner executes only its own graphs; the
//! store is read back on every batch, so a graph created for this runner by
//! any editor starts running here without a handshake.

mod cli;
mod feed;
mod jobs;
mod mux;
mod runner;
mod runs;
mod transport;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeughaus_core::{NodeId, Value, ZeughausError};
use zeughaus_link::{
    EVENTS_PATH, FEED_PATH, HOLD_PATH, MUX_PATH, RUNS_PATH, SNAPSHOT_PATH, Snapshot, TRIGGERS_PATH,
    TriggerRequest, credentials,
};
use zeughaus_runtime::DeferredWork;
use zeughaus_sync::Role;

use crate::feed::FrameRegistry;
use crate::jobs::JobHost;
use crate::mux::MuxService;
use crate::runner::{AsyncResult, Runner};
use crate::transport::Transport;

/// How long the loop blocks on the event channel before looking around.
///
/// The SDK delivers row changes over a `std::sync::mpsc` channel, which cannot
/// be selected over together with the async-result channel, so completed node
/// work is picked up between batches. This is therefore the worst-case latency
/// for an async result and for an ownership change -- short enough to be
/// imperceptible, long enough that an idle runner is free.
const TICK: Duration = Duration::from_millis(50);

/// Where the sample feed binds without `--feed-addr`. Port 0 lets the OS pick,
/// which is what makes two runners on one host possible; the port that was
/// actually bound is what gets announced.
const DEFAULT_FEED_ADDR: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

/// How many unserved trigger presses are held. A press is a moment, so a deep
/// queue of them is worth nothing: what a button needs is that the next press
/// is taken, not that the hundredth one from a loop still is.
const TRIGGER_BACKLOG: usize = 32;

/// How many presses one iteration serves. The loop that takes them also
/// applies graph changes and serves the clocks, so a flood must not be able to
/// hold it in the press queue; the rest wait for the next iteration, which is
/// at most `TICK` away.
const MAX_PRESSES_PER_TURN: usize = 8;

fn main() -> ExitCode {
    // A terminal's shim is this binary too, started by the mux with the
    // terminal's directory. It must be the first thing that happens: the
    // shim forks, and nothing may have started a thread before that.
    #[cfg(unix)]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() == Some(std::ffi::OsStr::new("shim")) {
            let Some(dir) = args.next() else {
                eprintln!("[shim] usage: zeughaus-runner shim <terminal-dir>");
                return ExitCode::FAILURE;
            };
            return zeughaus_terminal::shim::run(std::path::Path::new(&dir));
        }
    }
    // Every shell and job inherits the runner's environment, which may name
    // no locale at all (ssh, launchd, a stripped service).
    #[cfg(unix)]
    {
        let locale = zeughaus_terminal::locale::resolve();
        // SAFETY: nothing has started a thread yet; the shim branch above returned
        // and the store, runtime and transport are created further down.
        unsafe { locale.apply() };
        let vars: Vec<String> = locale
            .effective
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        eprintln!(
            "[runner] terminal locale ({}): {}",
            locale.source,
            vars.join(" ")
        );
        for (key, value, reason) in &locale.refused {
            eprintln!("[runner] terminal locale: ignoring {key}={value} ({reason})");
        }
    }
    // Resolved once, now: after a rebuild replaced the file, the running
    // image is "(deleted)" and only this path still names the new binary,
    // which is what a restart executes and what new shims are started from.
    let exe = std::env::current_exe();

    // A unique id range per process, so a runner that creates ids (none today,
    // but nodes may spawn nodes) can never collide with a live editor's.
    zeughaus_core::NodeId::seed_unique();
    zeughaus_core::EdgeId::seed_unique();

    let feed_addr = match parse_feed_addr() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("[runner] {e}");
            return ExitCode::FAILURE;
        }
    };
    let keep_runs = match parse_keep_runs() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[runner] {e}");
            return ExitCode::FAILURE;
        }
    };
    let state_dir = match parse_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("[runner] {e}");
            return ExitCode::FAILURE;
        }
    };
    // A subcommand makes this process a client of another runner and ends
    // here; only the state directory is shared with the serving path, for
    // the client identity it presents.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::run(&args, &state_dir) {
        return code;
    }
    eprintln!("[runner] state dir {}", state_dir.display());
    let session = zeughaus_sync::Session::resolve(parse_join_arg().as_deref());
    let (uri, db) = (session.uri, session.database);
    eprintln!("[runner] session {} -> {uri} / {db}", session.token);

    // A `Store` rather than a bare connection: a runner outlives a host
    // restart, and a dead connection it kept would leave it executing a graph
    // another runner has taken over.
    let (store, sync_rx) = match zeughaus_sync::Store::open(&uri, &db, Role::Runtime) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!(
                "[runner] cannot connect to {uri} / {db}: {e} (start it with `spacetime start`)"
            );
            return ExitCode::FAILURE;
        }
    };

    // Node work that has to run off the event loop, and the feed server. Four
    // workers because the feed's tasks live here: they park on QUIC flow
    // control and box-filter frames, which is milliseconds of CPU that must not
    // sit in front of quinn's driver on a single worker. Blocking node work
    // (LLM requests, screen capture) goes to the blocking pool from here, which
    // is what turns a panicking node into a reported error instead of a node
    // left pending forever.
    // The io driver is what quinn drives its UDP socket on and the time driver
    // is what a feed's frame-rate ceiling sleeps on; both panic at first use if
    // they were never enabled.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("zeughaus-async")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[runner] cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Bound before the runner exists, because the address is announced from the
    // runner's first look at the store and an editor must never be handed an
    // endpoint that is not serving yet.
    let frames = Arc::new(FrameRegistry::new());
    let transport = match rt.block_on(Transport::start(feed_addr, &state_dir)) {
        Ok(transport) => {
            eprintln!("[runner] weida endpoint {}", transport.url());
            // One line per key, because the answer to "why was my editor
            // refused" is a fingerprint comparison and nothing else says
            // which keys this process accepted.
            for fingerprint in transport.trusted_clients() {
                eprintln!("[runner] trusted client {fingerprint}");
            }
            Some(transport)
        }
        // Not fatal. The graph still executes; an editor simply sees no values
        // and says so, exactly as it does with no runner at all. Exiting here
        // would take the graph's execution away too.
        Err(e) => {
            eprintln!("[runner] no weida endpoint: {e}");
            None
        }
    };

    // `Publisher::publish` is synchronous, but every weida handle was created
    // on this runtime and quinn's driver has to be reachable from the loop
    // thread. The guard covers the whole loop below.
    let _enter = rt.enter();

    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    // Bounded: a press is a moment, and a peer that presses in a loop must not
    // be able to make this process grow. `accept_triggers` drops and says so
    // when it is full.
    let (trigger_tx, trigger_rx) = std::sync::mpsc::sync_channel::<TriggerRequest>(TRIGGER_BACKLOG);
    let mut publisher = None;
    // Jobs run in the mux, so a process without one has no host to offer them.
    let mut job_host: Option<Arc<JobHost>> = None;
    // Kept for the restart, which saves the workspace before it `exec`s.
    let mut mux_service: Option<MuxService> = None;
    if let Some(transport) = &transport {
        let listener = transport.listener();
        match listener.replier(FEED_PATH) {
            Ok(replier) => {
                rt.spawn(feed::accept_feeds(replier, Arc::clone(&frames)));
            }
            Err(e) => eprintln!("[runner] no sample feed: {e}"),
        }
        match listener.publisher(EVENTS_PATH) {
            Ok(events) => publisher = Some(events),
            Err(e) => eprintln!("[runner] no event stream: {e}"),
        }
        match listener.replier(SNAPSHOT_PATH) {
            Ok(replier) => {
                rt.spawn(transport::serve_snapshots(replier, Arc::clone(&snapshot)));
            }
            Err(e) => eprintln!("[runner] no snapshot service: {e}"),
        }
        match listener.puller(TRIGGERS_PATH) {
            Ok(puller) => {
                rt.spawn(transport::accept_triggers(puller, trigger_tx));
            }
            Err(e) => eprintln!("[runner] no trigger intake: {e}"),
        }
        // The terminal mux: one replier, three exchange kinds, every terminal
        // this process will ever run. Its incarnation is what tells an editor
        // that a restarted runner's terminals are not the ones it cached.
        match listener.replier(MUX_PATH) {
            Ok(replier) => {
                let service = MuxService::new(
                    mux::incarnation(),
                    Vec::new(),
                    terminal_host(exe.as_ref().ok(), &state_dir),
                );
                rt.spawn(service.clone().accept(replier));
                // A run is a terminal this process owns, so the job host is
                // the mux service plus the state directory its logs go in.
                let host = Arc::new(JobHost::new(service.clone(), state_dir.clone(), keep_runs));
                host.adopt_runs();
                job_host = Some(host);
                mux_service = Some(service);
            }
            Err(e) => eprintln!("[runner] no terminal mux: {e}"),
        }
        if let Some(host) = &job_host {
            match listener.replier(HOLD_PATH) {
                Ok(replier) => {
                    rt.spawn(jobs::serve_hold(replier, Arc::clone(host)));
                }
                Err(e) => eprintln!("[runner] no hold service: {e}"),
            }
        }
        // The runner that produced a run's files is the one that serves them:
        // they are on this disk and nowhere else.
        match listener.replier(RUNS_PATH) {
            Ok(replier) => {
                rt.spawn(runs::serve_runs(replier, state_dir.clone()));
            }
            Err(e) => eprintln!("[runner] no run file service: {e}"),
        }
    }

    // The fingerprint pinned in this runner's own URL is what a graph's
    // `runner` column names. Without an endpoint no editor could reach this
    // process, and no graph can name it.
    let fingerprint = transport.as_ref().and_then(|t| {
        weida::EndpointAddr::parse(t.url())
            .ok()
            .and_then(|addr| addr.peer)
            .map(|fp| fp.to_string())
    });
    if fingerprint.is_none() {
        eprintln!("[runner] no endpoint, so no graph is this runner's");
    }
    let mut runner = Runner::new(
        store,
        Arc::clone(&frames),
        publisher,
        Arc::clone(&snapshot),
        job_host.clone(),
        fingerprint,
    );
    if let Some(transport) = &transport {
        runner.set_endpoint(transport.url().to_string());
    }
    // The epoch travels with the work: a result that comes back after ownership
    // moved must not be applied, and the sender is the only place that knows
    // which ownership it was dispatched under.
    let (async_tx, async_rx) = std::sync::mpsc::channel::<(NodeId, u64, AsyncResult)>();

    // Stopping drains. A signal holds the host so no further run starts and
    // the loop leaves once the live ones have finished; the connection then
    // closes and the module drops this process's `runtime` row, which is the
    // handover a standby waits for. Killing it outright would take a live
    // build's terminal with it, which is the one thing a job must survive.
    let stopping = Arc::new(AtomicBool::new(false));
    {
        let stopping = Arc::clone(&stopping);
        let host = job_host.clone();
        rt.spawn(async move {
            wait_for_stop().await;
            if let Some(host) = &host {
                host.hold(true);
            }
            stopping.store(true, Ordering::SeqCst);
        });
    }
    // A restart replaces this process with the binary at `exe`, same PID,
    // same arguments. Nothing drains: shells and runs live in their shims,
    // and the next process reattaches them from the saved workspace.
    #[cfg(unix)]
    restart_signal::install();
    // Whether the draining line has been written: the loop turns twenty times
    // a second and the reason for waiting is worth saying once.
    let mut draining = false;
    // Whether the mux has seen the store's graphs since the subscription
    // applied. The first look is taken even when nothing changed, so a pane
    // of a graph deleted while this process was down goes too.
    let mut graphs_synced = false;
    loop {
        #[cfg(unix)]
        if restart_signal::take() {
            restart(exe.as_ref().ok(), mux_service.as_ref());
        }
        if stopping.load(Ordering::SeqCst) {
            let live = job_host.as_ref().map_or(0, |host| host.live_runs());
            if live == 0 {
                eprintln!("[runner] stopped");
                return ExitCode::SUCCESS;
            }
            if !draining {
                draining = true;
                eprintln!("[runner] draining {live} runs; new jobs refused");
            }
        }

        // Block for the first event, then take whatever else is already queued:
        // one pass per burst of row changes rather than one per row. A
        // subscription applying delivers a whole graph this way. The wait is
        // capped by whatever a clocked node is waiting for, so a 30 Hz timer is
        // served on time instead of at the polling interval.
        let mut events = Vec::new();
        match sync_rx.recv_timeout(runner.next_wait(TICK)) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Timeout) => {}
            // Every sender is held by the store, which this process owns for
            // its whole life, so this cannot happen while it is running -- and
            // if it ever did, nothing would ever be received again.
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[runner] the store channel closed, exiting");
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

        // A press is a request from an editor, so it is drained like any other
        // input to the pass. Latency is bounded by TICK, the same as an async
        // result -- and so is a press left over from a burst, because the loop
        // takes at most `MAX_PRESSES_PER_TURN` of them before it goes back to
        // serving the store and the clocks.
        let mut fired = false;
        let presses: Vec<TriggerRequest> = std::iter::from_fn(|| trigger_rx.try_recv().ok())
            .take(MAX_PRESSES_PER_TURN)
            .collect();

        // The store first: an outage costs this process its ownership, and
        // nothing below should decide anything on a connection that is gone.
        runner.poll_store();

        // Before anything is applied or published: a pass must not run on an
        // ownership this process no longer has.
        runner.refresh_ownership();

        // The store's events before any result or press: a result delivered
        // against a stale scope would still run the downstream nodes of a
        // node this batch deleted or moved out of this runner's graphs.
        let had_events = !events.is_empty();
        for event in events {
            runner.apply(event);
        }
        if had_events {
            runner.reconcile();
        }

        // Clocked nodes are what make a source a source: nothing upstream ever
        // wakes a screen capture, so without this the graph produces one frame
        // and stops.
        let ticked = runner.mark_due_ticks();

        for (node_id, epoch, result) in results {
            let deferred = runner.deliver(node_id, epoch, result);
            dispatch(&rt, &async_tx, runner.owner_epoch(), deferred);
        }

        for press in presses {
            fired |= runner.trigger(press.node_id, press.payload, press.external);
        }

        if had_events || ticked || fired {
            let deferred = runner.pass();
            dispatch(&rt, &async_tx, runner.owner_epoch(), deferred);
            // Only once the store's content is here: a partial picture would
            // close the panes of graphs that simply have not arrived yet.
            if runner.synced() && (runner.graphs_changed() || !graphs_synced) {
                graphs_synced = true;
                if let Some(mux) = &mux_service {
                    mux.sync_graphs(runner.graph_sync());
                }
            }
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
    tx: &Sender<(NodeId, u64, AsyncResult)>,
    epoch: u64,
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
            let _ = tx.send((node_id, epoch, outputs.map_err(|e| e.to_string())));
        });
    }
}

/// Resolves when this process is asked to stop.
///
/// Both signals mean the same thing here -- a terminal's Ctrl-C and a service
/// manager's `SIGTERM` -- and both are answered by draining rather than by
/// dying, which is why they are awaited instead of left to the default
/// disposition.
async fn wait_for_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            // No SIGTERM handler is not a reason to ignore Ctrl-C too.
            Err(e) => {
                eprintln!("[runner] no SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Where this runner's terminals live: in shims under
/// `<state-dir>/terminals` on unix, so they survive a restart, and in this
/// process otherwise. Without its own path the runner cannot start a shim.
fn terminal_host(
    exe: Option<&PathBuf>,
    state_dir: &std::path::Path,
) -> zeughaus_terminal::TerminalHost {
    #[cfg(unix)]
    match exe {
        Some(exe) => zeughaus_terminal::TerminalHost::Shim(zeughaus_terminal::ShimHost {
            program: exe.clone(),
            args: vec!["shim".into()],
            root: state_dir.join("terminals"),
        }),
        None => {
            eprintln!("[runner] cannot locate this executable; terminals end with the runner");
            zeughaus_terminal::TerminalHost::Local
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (exe, state_dir);
        zeughaus_terminal::TerminalHost::Local
    }
}

/// Saves the workspace and replaces this process with `exe`, same PID and
/// arguments. Returns only when that failed; the runner then carries on as
/// it was.
#[cfg(unix)]
fn restart(exe: Option<&PathBuf>, mux: Option<&MuxService>) {
    use std::os::unix::process::CommandExt;

    let Some(exe) = exe else {
        eprintln!("[runner] restart failed: this executable cannot be located");
        return;
    };
    if let Some(mux) = mux {
        mux.persist();
    }
    eprintln!("[runner] restarting");
    let e = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("[runner] restart failed: {e}");
}

/// `SIGUSR1` as a flag the loop reads every turn.
///
/// The restart has to happen on the loop's thread, between two turns, and
/// the loop already wakes at least every `TICK`: a handler that sets an
/// atomic is the whole mechanism, with no task, channel or runtime between
/// the signal and the `exec`.
#[cfg(unix)]
mod restart_signal {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_usr1(_: libc::c_int) {
        // An atomic store is async-signal-safe; nothing else happens here.
        REQUESTED.store(true, Ordering::SeqCst);
    }

    pub fn install() {
        // SAFETY: `on_usr1` only stores to an atomic, and `SA_RESTART` keeps
        // the signal from failing blocking calls elsewhere in the process.
        let installed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_usr1 as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) == 0
        };
        if !installed {
            eprintln!(
                "[runner] no restart signal: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Whether a restart was asked for since the last call.
    pub fn take() -> bool {
        REQUESTED.swap(false, Ordering::SeqCst)
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

/// Address the sample feed binds, from `--feed-addr <host:port>`.
///
/// Loopback and an OS-chosen port by default: the common case is an editor on
/// this machine, and a fixed port would collide between two runners on one host.
/// A remote viewer needs an explicit address, because the announced host is
/// taken from what was bound and loopback reaches nobody else.
fn parse_feed_addr() -> Result<SocketAddr, String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--feed-addr" {
            let text = args
                .next()
                .ok_or("--feed-addr needs a host:port argument")?;
            return feed_addr_from(&text);
        }
    }
    Ok(DEFAULT_FEED_ADDR)
}

fn feed_addr_from(text: &str) -> Result<SocketAddr, String> {
    text.parse()
        .map_err(|e| format!("--feed-addr {text:?} is not a host:port address: {e}"))
}

/// Where the runner's credentials live, from `--state-dir <path>`.
///
/// The flag exists for a second runner on one machine (a test fixture, a
/// service account): two processes sharing one `runner.pem` would announce the
/// same fingerprint from two ports, and an editor pooling by identity has no
/// way to tell them apart. Without it,
/// [`credentials::state_dir`](zeughaus_link::credentials::state_dir)
/// decides, which `ZEUGHAUS_STATE_DIR` already overrides.
fn parse_state_dir() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--state-dir" {
            let text = args.next().ok_or("--state-dir needs a path argument")?;
            return Ok(PathBuf::from(text));
        }
    }
    Ok(credentials::state_dir())
}

/// How many successful job runs stay on disk, from `--keep-runs <n>`.
///
/// Failed runs are never pruned, so this is the only knob: the log of a
/// green build is worth little once a newer green one exists, and a runner
/// that builds on every push would otherwise fill its state directory.
fn parse_keep_runs() -> Result<usize, String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--keep-runs" {
            let text = args.next().ok_or("--keep-runs needs a number")?;
            return text
                .parse()
                .map_err(|e| format!("--keep-runs {text:?} is not a number: {e}"));
        }
    }
    Ok(jobs::DEFAULT_KEEP_RUNS)
}
