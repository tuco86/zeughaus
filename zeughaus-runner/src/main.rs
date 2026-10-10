//! Headless graph runtime: the only process that executes a Zeughaus graph.
//!
//! It holds the graph document itself: it loads `<state-dir>/graphs/*.zgh` at
//! start, serves the document to editors on the graph link, applies every
//! change to a [`GraphExecutor`], persists the files and publishes the scalar
//! outputs it computes. Editors -- local or remote, native or browser -- edit
//! and display; they never run a node. That is what keeps a node with side
//! effects (a screen capture, an LLM request) firing once instead of once per
//! open window.
//!
//! This runner executes every graph of its own document. An editor attached
//! to several runners shows several documents side by side.

mod ci;
mod cli;
mod feed;
mod files;
mod graphs;
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
    BUSY_PATH, CI_PATH, EVENTS_PATH, FEED_PATH, GRAPH_PATH, GraphChange, HOLD_PATH, MUX_PATH,
    RUNS_PATH, SNAPSHOT_PATH, Snapshot, TRIGGERS_PATH, TriggerRequest, credentials,
};
use zeughaus_runtime::DeferredWork;

use crate::feed::FrameRegistry;
use crate::graphs::GraphService;
use crate::jobs::JobHost;
use crate::mux::MuxService;
use crate::runner::{AsyncResult, Runner};
use crate::transport::Transport;

/// How long the loop blocks on the change channel before looking around.
///
/// The channel cannot be selected over together with the async-result
/// channel, so completed node work is picked up between batches. This is
/// therefore the worst-case latency for an async result -- short enough to be
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
    let locale = {
        let locale = zeughaus_terminal::locale::resolve();
        // SAFETY: nothing has started a thread yet; the shim branch above returned
        // and the runtime and transport are created further down.
        unsafe { locale.apply() };
        locale
    };
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
    // Held for the life of the process, released by `exec` (the file is
    // close-on-exec) and taken again by the image that replaces it.
    #[cfg(unix)]
    let _state_lock = match lock_state_dir(&state_dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("[runner] {e}");
            return ExitCode::FAILURE;
        }
    };
    // Said only when serving: a subcommand's output is read by scripts.
    #[cfg(unix)]
    {
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
    eprintln!("[runner] state dir {}", state_dir.display());

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

    // Bound before the runner exists: an editor must never be handed an
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
            if let Err(e) = credentials::write_endpoint(&state_dir, transport.url()) {
                eprintln!("[runner] cannot write the endpoint file: {e}");
            }
            transport
        }
        // Fatal: without an endpoint no graph names this runner, no editor
        // reaches its terminals and no job runs, so a process left running
        // would only look alive. The service manager retries instead, which
        // is what a `--feed-addr` on an interface that is not up yet (WireGuard
        // at boot) needs.
        Err(e) => {
            eprintln!("[runner] no weida endpoint: {e}");
            return ExitCode::FAILURE;
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
    // CI's busy state, alerts and pipeline changes, reported to editors as
    // they happen.
    let mut ci_handle: Option<ci::Ci> = None;
    let (graphs, graph_rx, initial) = GraphService::load(&state_dir);
    // Registered with the other paths, answered only once the snapshot says
    // whether this runner runs CI: an editor that reconnects during startup
    // would otherwise see a CI runner without its machine and drop its view.
    let mut snapshot_replier = None;
    {
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
            Ok(replier) => snapshot_replier = Some(replier),
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
        // CI runs its pipelines as this host's jobs; without a `ci.toml` it
        // says so and does nothing, and there is no machine to override.
        if let Some(started) = job_host
            .as_ref()
            .and_then(|host| ci::start(Arc::clone(host), state_dir.clone()))
        {
            match listener.replier(BUSY_PATH) {
                Ok(replier) => {
                    rt.spawn(ci::busy::serve(replier, Arc::clone(&started.busy)));
                }
                Err(e) => eprintln!("[runner] no busy service: {e}"),
            }
            if let Some(mux) = &mux_service {
                match listener.replier(CI_PATH) {
                    Ok(replier) => {
                        let server = ci::serve::CiServer::new(
                            state_dir.clone(),
                            started.machines.clone(),
                            Arc::clone(&started.busy),
                            mux.clone(),
                            exe.as_ref().ok().cloned(),
                        );
                        rt.spawn(ci::serve::serve(replier, Arc::new(server)));
                    }
                    Err(e) => eprintln!("[runner] no ci service: {e}"),
                }
            }
            ci_handle = Some(started);
        }
        // The runner that produced a run's files is the one that serves them:
        // they are on this disk and nowhere else.
        match listener.replier(RUNS_PATH) {
            Ok(replier) => {
                rt.spawn(runs::serve_runs(replier, state_dir.clone()));
            }
            Err(e) => eprintln!("[runner] no run file service: {e}"),
        }
        match listener.replier(GRAPH_PATH) {
            Ok(replier) => {
                rt.spawn(graphs.clone().accept(replier));
            }
            Err(e) => eprintln!("[runner] no graph service: {e}"),
        }
    }

    let mut runner = Runner::new(
        Arc::clone(&frames),
        publisher,
        Arc::clone(&snapshot),
        job_host.clone(),
    );
    if let Some(ci) = &ci_handle {
        runner.report_machine(ci.busy.state());
    }
    if let Some(replier) = snapshot_replier {
        rt.spawn(transport::serve_snapshots(replier, Arc::clone(&snapshot)));
    }
    let (async_tx, async_rx) = std::sync::mpsc::channel::<(NodeId, AsyncResult)>();

    // The document as loaded: every node before any edge, in document order,
    // so an edge always finds both of its nodes.
    for node in initial.nodes {
        runner.apply(GraphChange::NodeUpsert { node });
    }
    for edge in initial.edges {
        runner.apply(GraphChange::EdgeInsert { edge });
    }
    // The mux takes its first look even when the document is empty, so a pane
    // of a graph that was deleted while this process was down goes too.
    runner.graphs_changed();
    if let Some(mux) = &mux_service {
        mux.sync_graphs(runner.graph_sync());
    }
    let deferred = runner.pass();
    dispatch(&rt, &async_tx, deferred);

    // Stopping drains. A signal holds the host so no further run starts and
    // the loop leaves once the live ones have finished. Killing it outright
    // would take a live build's terminal with it, which is the one thing a
    // job must survive.
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
    loop {
        #[cfg(unix)]
        if restart_signal::take() {
            restart(exe.as_ref().ok(), mux_service.as_ref(), &graphs);
        }
        if stopping.load(Ordering::SeqCst) {
            let live = job_host.as_ref().map_or(0, |host| host.live_runs());
            if live == 0 {
                graphs.persist(true);
                eprintln!("[runner] stopped");
                return ExitCode::SUCCESS;
            }
            if !draining {
                draining = true;
                eprintln!("[runner] draining {live} runs; new jobs refused");
            }
        }

        // Block for the first change, then take whatever else is already
        // queued: one pass per burst of edits rather than one per edit. The
        // wait is capped by whatever a clocked node is waiting for, so a 30 Hz
        // timer is served on time instead of at the polling interval.
        let mut changes = Vec::new();
        match graph_rx.recv_timeout(runner.next_wait(TICK)) {
            Ok(change) => changes.push(change),
            Err(RecvTimeoutError::Timeout) => {}
            // The sender is held by the graph service, which this process owns
            // for its whole life, so this cannot happen while it is running --
            // and if it ever did, nothing would ever be received again.
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[runner] the graph channel closed, exiting");
                return ExitCode::FAILURE;
            }
        }
        while let Ok(change) = graph_rx.try_recv() {
            changes.push(change);
        }

        let mut results = Vec::new();
        while let Ok(result) = async_rx.try_recv() {
            results.push(result);
        }

        // A press is a request from an editor, so it is drained like any other
        // input to the pass. Latency is bounded by TICK, the same as an async
        // result -- and so is a press left over from a burst, because the loop
        // takes at most `MAX_PRESSES_PER_TURN` of them before it goes back to
        // serving the graph and the clocks.
        let mut fired = false;
        let presses: Vec<TriggerRequest> = std::iter::from_fn(|| trigger_rx.try_recv().ok())
            .take(MAX_PRESSES_PER_TURN)
            .collect();

        // The changes before any result or press: a result delivered against a
        // stale graph would still run the downstream nodes of a node this
        // batch deleted.
        let had_changes = !changes.is_empty();
        for change in changes {
            runner.apply(change);
        }

        // Clocked nodes are what make a source a source: nothing upstream ever
        // wakes a screen capture, so without this the graph produces one frame
        // and stops.
        let ticked = runner.mark_due_ticks();

        for (node_id, result) in results {
            let deferred = runner.deliver(node_id, result);
            dispatch(&rt, &async_tx, deferred);
        }

        for press in presses {
            fired |= runner.trigger(press.node_id, press.payload, press.external);
        }

        if let Some(ci) = &ci_handle {
            runner.report_machine(ci.busy.state());
            while let Ok(alert) = ci.alerts.try_recv() {
                runner.report_ci_alert(alert);
            }
            while let Ok(change) = ci.changes.try_recv() {
                runner.report_ci_change(change);
            }
        }

        if had_changes || ticked || fired {
            let deferred = runner.pass();
            dispatch(&rt, &async_tx, deferred);
            if runner.graphs_changed()
                && let Some(mux) = &mux_service
            {
                mux.sync_graphs(runner.graph_sync());
            }
        }

        graphs.persist(false);
    }
}

/// Hands deferred node work to the blocking pool, reporting each outcome back
/// over `tx`.
///
/// The join handle is awaited on a runtime worker rather than here, because the
/// work takes seconds (an LLM request) and the event loop has to keep serving
/// edits meanwhile. A join failure is reported like any other error: the
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
fn restart(exe: Option<&PathBuf>, mux: Option<&MuxService>, graphs: &GraphService) {
    use std::os::unix::process::CommandExt;

    let Some(exe) = exe else {
        eprintln!("[runner] restart failed: this executable cannot be located");
        return;
    };
    graphs.persist(true);
    if let Some(mux) = mux {
        mux.persist();
    }
    eprintln!("[runner] restarting");
    let e = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("[runner] restart failed: {e}");
}

/// Takes `<state-dir>/runner.lock`, or says which process holds it.
///
/// Two runners on one state directory reattach the same shims, and a shim
/// serves only the newest connection: the second runner silently takes the
/// first one's terminals, and leaves them unreachable when it exits.
#[cfg(unix)]
fn lock_state_dir(state_dir: &std::path::Path) -> Result<std::fs::File, String> {
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsRawFd;

    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
    let path = state_dir.join("runner.lock");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock {
            return Err(format!("cannot lock {}: {err}", path.display()));
        }
        let mut holder = String::new();
        let _ = file.read_to_string(&mut holder);
        return Err(format!(
            "another runner (pid {}) serves {}; refusing to take its terminals",
            holder.trim(),
            state_dir.display()
        ));
    }
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = writeln!(file, "{}", std::process::id());
    Ok(file)
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
