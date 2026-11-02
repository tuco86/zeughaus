//! The mux service: the workspace's authority, the terminal sessions, the
//! control leases, and the exchanges that serve them on `/mux`.
//!
//! Three exchange kinds arrive on the one replier, told apart by their first
//! frame (see `zeughaus_mux::message`): a control exchange per client, a
//! terminal exchange per (client, terminal), and short row fetches. Every
//! exchange runs its request and reply directions concurrently, so a
//! client's keystroke never waits behind a large delta and a large delta
//! never waits behind a keystroke.
//!
//! The workspace lives in one mutex and every structural change goes through
//! it, so two clients splitting the same pane at once are serialized and
//! both get a consistent revision. Terminal output is never in that mutex:
//! each session has its own model lock, and a subscriber computes its delta
//! from the sequence number it last sent -- a slow client gets fewer,
//! larger deltas, never a queue of them.

#[cfg(unix)]
use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_job::{RunExit, read_run_end, record_run_end};
use zeughaus_link::{AGENT_VAR, ActivityEntry, ActivityKind, ActivityState, AgentStatus};
use zeughaus_mux::message::{Capability, Command, ErrorCode};
use zeughaus_mux::workspace::ProfileId;
use zeughaus_mux::{
    ClientHello, ClientInstanceId, CommandOutcome, CommandReply, ControlAttach, Controller,
    Dimensions, ExitState, MAJOR, MINOR, Message, RequestId, ResourceFetch, RowFetch,
    RunnerIncarnation, ServerHello, TerminalAttach, TerminalCommand, TerminalId, TopologyCommand,
    WireError, WorkspaceSnapshot,
};
use zeughaus_terminal::frames::{ReadError, read_frame, write_frame};
use zeughaus_terminal::serve::{fetch_resource, fetch_rows, stream_terminal};
use zeughaus_terminal::wal::{self, Wal, WalWriter};
use zeughaus_terminal::{Profile, Session, TerminalHost};

use super::close;
use super::persist::{self, SavedRun, SavedTerminal};
use super::workspace::Workspace;
use super::{GraphSync, OwnedPlacement};
use crate::runs::{RunActivity, RunDirs, read_activity, write_activity};
use crate::transport::principal_of;

/// How often the reaper looks for terminals whose child exited.
const REAP_EVERY: Duration = Duration::from_millis(250);

/// Every this many sweeps (2 s) the reaper also looks for agents whose
/// process is gone.
#[cfg(unix)]
const AGENT_CHECK_SWEEPS: u64 = 8;

/// Closed terminals whose end [`MuxService::terminal_state`] remembers.
#[cfg(unix)]
const CLOSED_KEPT: usize = 256;

/// How long after a client vanishes its leases are held for it, so a network
/// blink does not make its terminals read-only.
const LEASE_GRACE: Duration = Duration::from_secs(10);

/// Replies remembered per client for deduplicating a command resent after a
/// redial.
const REMEMBERED_REPLIES: usize = 64;

/// Most clients attached at once. Beyond it an attach is refused.
const MAX_CLIENTS: usize = 64;

/// Who controls a terminal, and until when that is still theirs after they
/// went away.
struct Lease {
    client: ClientInstanceId,
    principal: String,
    /// Set when the client's control exchange ended; `None` while it is
    /// attached.
    detached_at: Option<Instant>,
}

impl Lease {
    fn controller(&self) -> Controller {
        Controller {
            client: self.client,
            principal: self.principal.clone(),
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.detached_at
            .is_some_and(|at| now.duration_since(at) >= LEASE_GRACE)
    }
}

struct ClientState {
    principal: String,
    /// Replies already given, by request id: a command resent after a
    /// redial is answered from here, not applied twice.
    replies: BTreeMap<RequestId, CommandOutcome>,
}

/// What a restarted runner needs to reattach a terminal besides its id,
/// and what it needs to finish a job run that outlived its starter.
struct TerminalMeta {
    label: String,
    scrollback_rows: usize,
    run: Option<SavedRun>,
    kind: ActivityKind,
    /// Unix seconds.
    started: u64,
    /// A closed terminal's output opened for reading: never saved, never
    /// archived, never reported as activity.
    replay: bool,
}

/// The document's graphs as far as the panes that show them care.
#[derive(Default)]
struct Graphs {
    /// This runner's top-level graphs that already had their chance at a
    /// tab in this process. Not saved: after a restart every graph of this
    /// runner's that no pane shows gets a tab again.
    seen: BTreeSet<u64>,
    /// Every node's display name, for tabs named after a graph.
    names: HashMap<u64, String>,
    /// Every node id in the document; `None` until the first sync.
    exists: Option<HashSet<u64>>,
}

struct Inner {
    incarnation: RunnerIncarnation,
    profiles: Vec<Profile>,
    host: TerminalHost,
    workspace: Mutex<Workspace>,
    sessions: Mutex<HashMap<TerminalId, Session>>,
    /// Taken after `sessions` and `workspace` when all three are needed.
    meta: Mutex<HashMap<TerminalId, TerminalMeta>>,
    /// Taken after `workspace`.
    graphs: Mutex<Graphs>,
    leases: Mutex<HashMap<TerminalId, Lease>>,
    clients: Mutex<HashMap<ClientInstanceId, ClientState>>,
    next_terminal: AtomicU64,
    /// The workspace revision, for control exchanges to wake on.
    revision: watch::Sender<u64>,
    /// Where closed shells and sessions are archived.
    run_dirs: Arc<RunDirs>,
    /// Where activity goes; set when the reaper starts.
    activity: OnceLock<Sender<ActivityEntry>>,
    /// The newest [`CLOSED_KEPT`] terminals that were closed, with how their
    /// child had ended: what an owner that looks after the close learns.
    #[cfg(unix)]
    closed: Mutex<VecDeque<(TerminalId, Option<ExitState>)>>,
    /// Agent statuses whose process is gone, by terminal and the status's
    /// `at`: reported as no status until the agent reports a new one.
    gone_agents: Mutex<HashSet<(TerminalId, u64)>>,
    /// Builds the profile of a work shell for an origin and branch; unset
    /// on a runner that opens no projects.
    project_profile: OnceLock<ProjectProfile>,
}

/// The profile of a shell in the work checkout of `origin`, checked out at
/// `branch` when it has to be cloned first.
pub type ProjectProfile = Arc<dyn Fn(&str, &str) -> Result<Profile, String> + Send + Sync>;

impl Inner {
    fn publish_revision(&self, revision: u64) {
        // PTY readers and topology commands publish concurrently, after
        // releasing the workspace lock. A late publisher must not rewind it.
        self.revision.send_if_modified(|current| {
            if revision > *current {
                *current = revision;
                true
            } else {
                false
            }
        });
    }
}

/// The service handle. Cheap to clone; one per runner.
#[derive(Clone)]
pub struct MuxService {
    inner: Arc<Inner>,
}

impl MuxService {
    /// The service over `host`. With shims, the terminals a previous runner
    /// left in them are reattached and the workspace that showed them is
    /// restored around the ones that came back.
    pub fn new(
        incarnation: RunnerIncarnation,
        profiles: Vec<Profile>,
        host: TerminalHost,
        run_dirs: Arc<RunDirs>,
    ) -> MuxService {
        let profiles = if profiles.is_empty() {
            vec![Profile::default_shell()]
        } else {
            profiles
        };
        let (revision, _) = watch::channel(1);
        let service = MuxService {
            inner: Arc::new(Inner {
                incarnation,
                profiles,
                host,
                workspace: Mutex::new(Workspace::new(incarnation)),
                sessions: Mutex::new(HashMap::new()),
                meta: Mutex::new(HashMap::new()),
                graphs: Mutex::new(Graphs::default()),
                leases: Mutex::new(HashMap::new()),
                clients: Mutex::new(HashMap::new()),
                next_terminal: AtomicU64::new(1),
                revision,
                run_dirs,
                activity: OnceLock::new(),
                #[cfg(unix)]
                closed: Mutex::new(VecDeque::new()),
                gone_agents: Mutex::new(HashSet::new()),
                project_profile: OnceLock::new(),
            }),
        };
        #[cfg(unix)]
        if let TerminalHost::Shim(shim) = &service.inner.host {
            service.restore(shim);
        }
        service
    }

    /// Where the workspace is saved: beside the shims' root. `None` when
    /// terminals die with this process and there is nothing to restore.
    fn state_file(&self) -> Option<std::path::PathBuf> {
        match &self.inner.host {
            TerminalHost::Local => None,
            #[cfg(unix)]
            TerminalHost::Shim(shim) => Some(shim.root.with_file_name("workspace.json")),
        }
    }

    /// How [`TopologyCommand::OpenProject`] builds its shell. A second call
    /// is ignored. Work shells are a Unix exec, like agent sessions.
    #[cfg(unix)]
    pub fn set_project_profile(&self, f: ProjectProfile) {
        let _ = self.inner.project_profile.set(f);
    }

    #[cfg(unix)]
    fn restore(&self, shim: &zeughaus_terminal::ShimHost) {
        let saved = self.state_file().and_then(|path| persist::load(&path));
        let mut restored: HashMap<TerminalId, Session> = HashMap::new();
        let mut meta: HashMap<TerminalId, TerminalMeta> = HashMap::new();
        for terminal in saved.iter().flat_map(|saved| &saved.terminals) {
            match Session::reattach(
                terminal.id,
                shim,
                &terminal.label,
                terminal.scrollback_rows,
                terminal.size,
            ) {
                Ok(session) => {
                    self.follow_title(&session);
                    self.follow_agent(&session);
                    restored.insert(terminal.id, session);
                    meta.insert(
                        terminal.id,
                        TerminalMeta {
                            label: terminal.label.clone(),
                            scrollback_rows: terminal.scrollback_rows,
                            run: terminal.run.clone(),
                            kind: terminal.kind.unwrap_or(if terminal.run.is_some() {
                                ActivityKind::Job
                            } else {
                                ActivityKind::Shell
                            }),
                            started: terminal.started,
                            replay: false,
                        },
                    );
                }
                Err(e) => eprintln!("[mux] terminal {} not restored: {e}", terminal.id.0),
            }
        }
        let workspace = match &saved {
            Some(saved) => Workspace::restore(self.inner.incarnation, saved, |id| {
                restored.contains_key(&id)
            }),
            None => Workspace::new(self.inner.incarnation),
        };
        // A terminal no pane shows and the runner does not own would be a
        // shell nobody can ever reach again.
        restored.retain(|id, session| {
            let known = workspace.knows(*id);
            if !known {
                session.kill();
                meta.remove(id);
            }
            known
        });
        // Shims nothing refers to any more: a terminal that did not
        // reattach, or one whose workspace entry was lost with the file.
        let mut highest_dir = 0;
        if let Ok(entries) = std::fs::read_dir(&shim.root) {
            for entry in entries.flatten() {
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u64>().ok())
                else {
                    continue;
                };
                highest_dir = highest_dir.max(id);
                if !restored.contains_key(&TerminalId(id)) {
                    zeughaus_terminal::shim::close_dir(&entry.path());
                }
            }
        }
        let highest_restored = restored.keys().map(|id| id.0).max().unwrap_or(0);
        let next = saved
            .as_ref()
            .map_or(1, |saved| saved.next_terminal)
            .max(highest_dir + 1)
            .max(highest_restored + 1);
        self.inner.next_terminal.store(next, Ordering::Relaxed);
        if !restored.is_empty() {
            eprintln!("[mux] restored {} terminals", restored.len());
        }
        *self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = restored;
        *self
            .inner
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = workspace;
        *self.inner.meta.lock().unwrap_or_else(|e| e.into_inner()) = meta;
        self.persist();
    }

    /// Saves the workspace and its terminals for the next runner. Nothing
    /// to do when terminals end with this process.
    pub fn persist(&self) {
        let Some(path) = self.state_file() else {
            return;
        };
        let saved = {
            // The lock order `snapshot` takes, and `meta` last.
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let meta = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
            let mut terminals: Vec<SavedTerminal> = sessions
                .iter()
                .filter_map(|(id, session)| {
                    let meta = meta.get(id).filter(|meta| !meta.replay)?;
                    Some(SavedTerminal {
                        id: *id,
                        label: meta.label.clone(),
                        scrollback_rows: meta.scrollback_rows,
                        size: session.size(),
                        run: meta.run.clone(),
                        kind: Some(meta.kind),
                        started: meta.started,
                    })
                })
                .collect();
            terminals.sort_by_key(|t| t.id);
            workspace.to_saved(self.inner.next_terminal.load(Ordering::Relaxed), terminals)
        };
        if let Err(e) = persist::store(&path, &saved) {
            eprintln!("[mux] cannot save {}: {e}", path.display());
        }
    }

    /// The job runs among the terminals this process reattached: runs a
    /// previous runner started and never saw end.
    pub fn restored_runs(&self) -> Vec<(TerminalId, SavedRun)> {
        let meta = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
        let mut runs: Vec<(TerminalId, SavedRun)> = meta
            .iter()
            .filter_map(|(id, meta)| Some((*id, meta.run.clone()?)))
            .collect();
        runs.sort_by_key(|(id, _)| *id);
        runs
    }

    /// The terminal that runs the job whose run directory is `run_dir`, if it
    /// still exists.
    pub fn terminal_of_run(&self, run_dir: &std::path::Path) -> Option<TerminalId> {
        let meta = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
        meta.iter()
            .find(|(_, meta)| meta.run.as_ref().is_some_and(|run| run.run_dir == run_dir))
            .map(|(id, _)| *id)
    }

    /// Whether terminal `id` still runs (`Some(None)`), how its child ended
    /// (`Some(Some(exit))`), or that it is gone without an exit, or too long
    /// ago to remember (`None`). Only agent sessions ask, on Unix.
    #[cfg(unix)]
    pub fn terminal_state(&self, id: TerminalId) -> Option<Option<ExitState>> {
        if let Some(session) = self.session(id) {
            return Some(session.exit());
        }
        let closed = self.inner.closed.lock().unwrap_or_else(|e| e.into_inner());
        closed
            .iter()
            .rev()
            .find(|(t, _)| *t == id)
            .and_then(|(_, exit)| exit.clone())
            .map(Some)
    }

    /// Starts a terminal the runner owns: a job's process, not a pane's
    /// shell. `placement` says whether a pane shows it from the start; a
    /// detached one waits for a client to attach it. Closing a pane that
    /// shows it detaches it again instead of ending the child; only
    /// [`MuxService::close_terminal`], closing its tab in the locked group
    /// and the runner's exit do that.
    ///
    /// `wal` records every byte the program writes with the time it was
    /// read, which is how a run's output is kept; `run` is what a restarted
    /// runner needs to finish the run's record. `size` is the grid until a
    /// pane that controls it resizes it.
    pub fn spawn_owned(
        &self,
        mut profile: Profile,
        wal: Wal,
        size: Dimensions,
        run: Option<SavedRun>,
        placement: OwnedPlacement,
        kind: ActivityKind,
    ) -> Result<TerminalId, String> {
        let id = TerminalId(self.inner.next_terminal.fetch_add(1, Ordering::Relaxed));
        profile
            .env
            .push(("ZEUGHAUS_TERMINAL".to_owned(), id.0.to_string()));
        let session = Session::spawn(id, &profile, size, wal, &self.inner.host)
            .map_err(|e| format!("cannot start {}: {e}", profile.label))?;
        self.follow_title(&session);
        self.follow_agent(&session);
        let meta = TerminalMeta {
            label: profile.label,
            scrollback_rows: profile.scrollback_rows,
            run,
            kind,
            started: now_secs(),
            replay: false,
        };
        let running = self.entry(id, &meta, ActivityState::Running, None, None);
        let revision = {
            // The lock order is the one `snapshot` takes: sessions, then
            // the workspace.
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            sessions.insert(id, session);
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            workspace.add_owned(id, placement);
            self.inner
                .meta
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, meta);
            workspace.revision()
        };
        self.inner.publish_revision(revision);
        self.persist();
        self.emit(running);
        Ok(id)
    }

    /// Starts the thread that notices exited children: it reports each
    /// exit on `activity` (where every other activity goes from now on
    /// too) and closes a shell or session whose exit `close_on_exit` in
    /// `<state-dir>/zeughaus.toml` says to close. Its first sweep handles
    /// the terminals that exited while no runner watched.
    pub fn start_reaper(&self, state_dir: PathBuf, activity: Sender<ActivityEntry>) {
        let _ = self.inner.activity.set(activity);
        let service = self.clone();
        let started = std::thread::Builder::new()
            .name("zeughaus-reaper".to_owned())
            .spawn(move || {
                let mut seen: HashSet<TerminalId> = HashSet::new();
                let mut sweep: u64 = 0;
                loop {
                    service.reap(&state_dir, &mut seen, sweep);
                    sweep = sweep.wrapping_add(1);
                    std::thread::sleep(REAP_EVERY);
                }
            });
        if let Err(e) = started {
            eprintln!("[mux] cannot start the reaper: {e}");
        }
    }

    /// One sweep of the reaper. `seen` holds the terminals whose exit was
    /// handled already.
    fn reap(&self, state_dir: &Path, seen: &mut HashSet<TerminalId>, sweep: u64) {
        #[cfg(unix)]
        if sweep.is_multiple_of(AGENT_CHECK_SWEEPS) {
            self.reap_agents();
        }
        #[cfg(not(unix))]
        let _ = sweep;
        let exited: Vec<(TerminalId, ExitState)> = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            seen.retain(|id| sessions.contains_key(id));
            sessions
                .iter()
                .filter(|(id, _)| !seen.contains(id))
                .filter_map(|(id, session)| Some((*id, session.exit()?)))
                .collect()
        };
        if exited.is_empty() {
            return;
        }
        let policy = close::policy(state_dir);
        for (id, exit) in exited {
            seen.insert(id);
            let found = {
                let metas = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
                metas.get(&id).map(|meta| {
                    (
                        self.entry(id, meta, ActivityState::Exited, Some(&exit), None),
                        meta.run.is_some(),
                        meta.replay,
                    )
                })
            };
            let Some((entry, has_run, replay)) = found else {
                continue;
            };
            if !replay {
                self.emit(entry);
            }
            if close::closes(policy, &exit, has_run, replay)
                && let Err(e) = self.close_terminal(id)
            {
                eprintln!("[mux] cannot close exited {id}: {e}");
            }
        }
    }

    /// Drops the status of every agent on this host whose process is gone:
    /// one killed without saying so leaves its last status behind.
    #[cfg(unix)]
    fn reap_agents(&self) {
        let host = local_hostname();
        let gone: Vec<(TerminalId, AgentStatus)> = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let known = self
                .inner
                .gone_agents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            sessions
                .iter()
                .filter(|(_, session)| session.exit().is_none())
                .filter_map(|(id, session)| Some((*id, agent_status(session)?)))
                .filter(|(id, status)| {
                    !known.contains(&(*id, status.at))
                        && status.host == host
                        && status.pid.is_some_and(process_gone)
                })
                .collect()
        };
        if gone.is_empty() {
            return;
        }
        self.inner
            .gone_agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(gone.iter().map(|(id, status)| (*id, status.at)));
        let entries: Vec<ActivityEntry> = {
            let metas = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
            gone.iter()
                .filter_map(|(id, _)| {
                    let meta = metas.get(id).filter(|meta| !meta.replay)?;
                    Some(self.entry(*id, meta, ActivityState::Running, None, None))
                })
                .collect()
        };
        for entry in entries {
            self.emit(entry);
        }
    }

    /// Sends `entry` to the activity channel, once there is one.
    fn emit(&self, entry: ActivityEntry) {
        if let Some(tx) = self.inner.activity.get() {
            let _ = tx.send(entry);
        }
    }

    /// The run directory id of a job's terminal.
    fn run_of(&self, meta: &TerminalMeta) -> Option<u64> {
        meta.run
            .as_ref()
            .and_then(|run| self.inner.run_dirs.id_of(&run.run_dir))
    }

    /// What activity says about terminal `id` in `state`. A terminal closed
    /// without an exit code counts as killed. An `agent` whose process is
    /// known to be gone is left out.
    fn entry(
        &self,
        id: TerminalId,
        meta: &TerminalMeta,
        state: ActivityState,
        exit: Option<&ExitState>,
        agent: Option<AgentStatus>,
    ) -> ActivityEntry {
        let code = match exit {
            Some(ExitState::Exited { code }) => i32::try_from(*code).ok(),
            _ => None,
        };
        let killed = match state {
            ActivityState::Closed => !matches!(exit, Some(ExitState::Exited { .. })),
            _ => matches!(exit, Some(ExitState::Killed)),
        };
        let agent = agent.filter(|status| {
            !self
                .inner
                .gone_agents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&(id, status.at))
        });
        ActivityEntry {
            terminal: id.0,
            run: self.run_of(meta),
            kind: meta.kind,
            label: meta.label.clone(),
            state,
            started: meta.started,
            ended: (state != ActivityState::Running).then(now_secs),
            code,
            killed,
            agent,
        }
    }

    /// Every live terminal but the replays, newest first.
    pub fn activity(&self) -> Vec<ActivityEntry> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let metas = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<ActivityEntry> = sessions
            .iter()
            .filter_map(|(id, session)| {
                let meta = metas.get(id).filter(|meta| !meta.replay)?;
                let exit = session.exit();
                let state = if exit.is_some() {
                    ActivityState::Exited
                } else {
                    ActivityState::Running
                };
                let agent = agent_status(session);
                Some(self.entry(*id, meta, state, exit.as_ref(), agent))
            })
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse((e.started, e.terminal)));
        entries
    }

    /// The shim directory of terminal `id`; `None` when terminals have no
    /// directory (they die with this process).
    fn shim_dir(&self, id: TerminalId) -> Option<PathBuf> {
        match &self.inner.host {
            TerminalHost::Local => {
                let _ = id;
                None
            }
            #[cfg(unix)]
            TerminalHost::Shim(shim) => Some(shim.dir(id)),
        }
    }

    /// Keeps a closing shell's or session's output as a run of its own:
    /// its WAL, its exit and what it was. Called before the session is
    /// killed, because the shim removes its directory with it. `None` when
    /// there is nothing to keep it from.
    fn archive(
        &self,
        id: TerminalId,
        meta: &TerminalMeta,
        exit: Option<&ExitState>,
        cols: u16,
    ) -> Option<u64> {
        let shim = self.shim_dir(id)?;
        let (run, dir) = match self.inner.run_dirs.create() {
            Ok(created) => created,
            Err(e) => {
                eprintln!("[mux] {id}: cannot archive: {e}");
                return None;
            }
        };
        if let Err(e) = copy_wal(&shim, &dir.join("wal")) {
            eprintln!("[mux] {id}: cannot archive its output: {e}");
        }
        let end = match exit {
            Some(ExitState::Exited { code }) => RunExit {
                code: Some(i32::try_from(*code).unwrap_or(-1)),
                killed: false,
            },
            _ => RunExit {
                code: None,
                killed: true,
            },
        };
        if let Err(e) = record_run_end(&dir, end, meta.started, None, &[]) {
            eprintln!("[mux] {id}: {e}");
        }
        let activity = RunActivity {
            kind: meta.kind,
            label: meta.label.clone(),
            terminal: id.0,
            cols,
        };
        if let Err(e) = write_activity(&dir, &activity) {
            eprintln!("[mux] {id}: {e}");
        }
        Some(run)
    }

    /// Opens archived run `run` read-only in a tab of its own.
    fn replay(&self, run: u64) -> CommandOutcome {
        let refused = |reason: String| CommandOutcome::Refused { reason };
        let dir = self.inner.run_dirs.dir(run);
        let records = match wal::read(&dir.join("wal")) {
            Ok(records) => records,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return refused(format!("run {run} has no output"));
            }
            Err(e) => return refused(format!("run {run}: {e}")),
        };
        let activity = read_activity(&dir);
        let cols = activity.as_ref().map_or(80, |a| a.cols);
        let label = activity
            .as_ref()
            .map_or_else(|| format!("run {run}"), |a| a.label.clone());
        let title = format!("{label} (closed)");
        let id = TerminalId(self.inner.next_terminal.fetch_add(1, Ordering::Relaxed));
        let session = Session::fed(
            id,
            Dimensions { cols, rows: 24 },
            crate::jobs::RUN_SCROLLBACK_ROWS,
            &title,
        );
        for record in &records {
            session.feed(&record.bytes);
        }
        session.finish(match read_run_end(&dir) {
            Some(RunExit {
                code: Some(code), ..
            }) if code >= 0 => ExitState::Exited { code: code as u32 },
            _ => ExitState::Killed,
        });
        match self.insert_fed(session, title) {
            Ok(_) => CommandOutcome::Applied {
                revision: *self.inner.revision.borrow(),
            },
            Err(reason) => refused(reason),
        }
    }

    /// Shows `session`, a fed one, in a tab at the end of the top level.
    /// Closing the tab ends it and keeps nothing.
    pub fn insert_fed(&self, session: Session, title: String) -> Result<TerminalId, String> {
        let id = session.id();
        let revision = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            workspace.add_tab(id, title.clone())?;
            sessions.insert(id, session);
            self.inner
                .meta
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id,
                    TerminalMeta {
                        label: title,
                        scrollback_rows: crate::jobs::RUN_SCROLLBACK_ROWS,
                        run: None,
                        kind: ActivityKind::Shell,
                        started: now_secs(),
                        replay: true,
                    },
                );
            workspace.revision()
        };
        self.inner.publish_revision(revision);
        self.persist();
        Ok(id)
    }

    /// Brings the graph panes in line with the document, as
    /// [`Workspace::sync_graphs`] describes, and keeps the names tabs are
    /// titled with and the ids `OpenGraph` is checked against.
    pub fn sync_graphs(&self, update: GraphSync) {
        let revision = {
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut graphs = self.inner.graphs.lock().unwrap_or_else(|e| e.into_inner());
            let graphs = &mut *graphs;
            let changed = workspace.sync_graphs(&update, &graphs.names, &mut graphs.seen);
            graphs.names = update.names;
            graphs.exists = Some(update.exists);
            changed.then(|| workspace.revision())
        };
        if let Some(revision) = revision {
            self.inner.publish_revision(revision);
            self.persist();
        }
    }

    /// Ends a terminal and forgets it, whether a pane shows it or not: a
    /// terminal the runner owns the way a client's `CloseTerminal` does, a
    /// pane's shell by closing its pane.
    pub fn close_terminal(&self, terminal: TerminalId) -> Result<(), String> {
        let command = {
            let workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match workspace.pane_of(terminal) {
                Some(pane) if !workspace.owns(terminal) => TopologyCommand::ClosePane { pane },
                _ => TopologyCommand::CloseTerminal { terminal },
            }
        };
        match self.structural(&command) {
            CommandOutcome::Applied { .. } => Ok(()),
            CommandOutcome::Refused { reason } => Err(reason),
        }
    }

    /// Accepts exchanges until the replier goes away, which for this process
    /// means never.
    pub async fn accept(self, replier: Replier) {
        loop {
            match replier.accept().await {
                Ok(request) => {
                    let service = self.clone();
                    tokio::spawn(async move { service.serve(request).await });
                }
                Err(e) => {
                    eprintln!("[mux] stopped accepting: {e}");
                    return;
                }
            }
        }
    }

    /// One exchange: the first frame says which kind.
    async fn serve(self, mut request: IncomingRequest) {
        let Some(principal) = principal_of(request.meta()) else {
            eprintln!("[mux] refused an exchange from a peer that proved no identity");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        };
        let mut body = request.take_body();
        let canceled = request.canceled();
        let first = match read_frame(&mut body).await {
            Ok((_, message)) => message,
            Err(e) => {
                eprintln!("[mux] {principal}: unreadable first frame: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[mux] {principal}: cannot open the reply half: {e}");
                return;
            }
        };
        let outcome = match first {
            Message::ControlAttach(attach) => {
                self.serve_control(principal, attach, body, &mut reply, canceled)
                    .await
            }
            Message::TerminalAttach(attach) => {
                self.serve_terminal(principal, attach, body, &mut reply, canceled)
                    .await
            }
            Message::RowFetch(fetch) => self.serve_fetch(fetch, &mut reply).await,
            Message::ResourceFetch(fetch) => self.serve_resource(fetch, &mut reply).await,
            other => Err(WireError {
                code: ErrorCode::Protocol,
                message: format!("{:?} cannot open an exchange", other.kind()),
            }),
        };
        if let Err(error) = outcome {
            // Best effort: the peer may already be gone, and then there is
            // nobody to tell.
            let _ = write_frame(&mut reply, &Message::Error(error), 0).await;
        }
        let _ = reply.finish();
    }

    async fn serve_control(
        &self,
        principal: String,
        attach: ControlAttach,
        mut body: weida::IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
        canceled: impl Future<Output = ()>,
    ) -> Result<(), WireError> {
        let hello = attach.hello;
        if hello.major != MAJOR {
            return Err(WireError {
                code: ErrorCode::Version,
                message: format!("protocol {} is not {MAJOR}", hello.major),
            });
        }
        let client = hello.client;
        self.register(client, &principal)?;
        let mut revision = self.inner.revision.subscribe();
        revision.borrow_and_update();
        let attached = self.attached(&hello, &principal);
        let mut sent_revision = attached.workspace.revision;
        write_frame(reply, &Message::ControlAttached(attached), 0)
            .await
            .map_err(|e| gone(&e))?;

        let mut canceled = std::pin::pin!(canceled);
        let result = loop {
            tokio::select! {
                () = &mut canceled => break Ok(()),
                changed = revision.changed() => {
                    if changed.is_err() {
                        break Ok(());
                    }
                    let current = *revision.borrow_and_update();
                    if current > sent_revision {
                        let snapshot = self.snapshot();
                        sent_revision = snapshot.revision;
                        if let Err(e) = write_frame(reply, &Message::WorkspaceSnapshot(snapshot), 0).await {
                            break Err(gone(&e));
                        }
                    }
                }
                frame = read_frame(&mut body) => {
                    let message = match frame {
                        Ok((_, message)) => message,
                        Err(ReadError::Ended) | Err(ReadError::Io(_)) => break Ok(()),
                        Err(ReadError::Codec(e)) => break Err(WireError {
                            code: ErrorCode::Protocol,
                            message: e.to_string(),
                        }),
                    };
                    let Message::Command(command) = message else {
                        break Err(WireError {
                            code: ErrorCode::Protocol,
                            message: format!("{:?} on a control exchange", message.kind()),
                        });
                    };
                    let outcome = self.command(client, &principal, &command);
                    let reply_message = Message::CommandReply(CommandReply {
                        request: command.request,
                        outcome,
                    });
                    if let Err(e) = write_frame(reply, &reply_message, command.request.0).await {
                        break Err(gone(&e));
                    }
                }
            }
        };
        self.detach(client);
        result
    }

    async fn serve_terminal(
        &self,
        principal: String,
        attach: TerminalAttach,
        mut body: weida::IncomingTransfer,
        reply: &mut weida::OutgoingTransfer,
        canceled: impl Future<Output = ()>,
    ) -> Result<(), WireError> {
        let client = attach.client;
        let terminal = attach.terminal;
        let session = self.session(terminal).ok_or(WireError {
            code: ErrorCode::UnknownTerminal,
            message: format!("no {terminal}"),
        })?;
        // Serials are per client and per exchange; the streaming half stamps
        // every delta with the highest one handled here.
        let acked_serial = AtomicU64::new(0);

        let streaming = stream_terminal(
            &session,
            &attach,
            &acked_serial,
            reply,
            || {
                if self.controls(terminal, client) {
                    let _ = session.apply(&TerminalCommand::Resize {
                        size: attach.size,
                        cell: session.cell(),
                    });
                }
            },
            || !self.exists(terminal),
        );
        let reading = async {
            loop {
                let message = match read_frame(&mut body).await {
                    Ok((_, message)) => message,
                    Err(ReadError::Ended) | Err(ReadError::Io(_)) => return Ok(()),
                    Err(ReadError::Codec(e)) => {
                        return Err(WireError {
                            code: ErrorCode::Protocol,
                            message: e.to_string(),
                        });
                    }
                };
                let Message::TerminalCommand(command) = message else {
                    return Err(WireError {
                        code: ErrorCode::Protocol,
                        message: format!("{:?} on a terminal exchange", message.kind()),
                    });
                };
                match command {
                    TerminalCommand::Viewport { .. } => {}
                    command @ TerminalCommand::Resize { .. } => {
                        if self.acquire_if_free(terminal, client, &principal, &session)
                            && let Err(e) = session.apply(&command)
                        {
                            eprintln!("[mux] {terminal}: resize refused: {e}");
                        }
                    }
                    input => {
                        // Acknowledged whether or not it is applied: a
                        // viewer's keystroke is handled by being dropped,
                        // and its cursor must not wait for it forever.
                        if let Some(serial) = input.serial() {
                            acked_serial.fetch_max(serial, Ordering::SeqCst);
                        }
                        // The first client that types acquires an unowned
                        // terminal; a viewer's input is dropped, never
                        // queued -- taking control is an explicit command.
                        if self.acquire_if_free(terminal, client, &principal, &session)
                            && let Err(e) = session.apply(&input)
                        {
                            eprintln!("[mux] {terminal}: input refused: {e}");
                        }
                    }
                }
            }
        };
        tokio::select! {
            () = canceled => Ok(()),
            result = streaming => result.map_err(|e| gone(&e)),
            result = reading => result,
        }
    }

    async fn serve_fetch(
        &self,
        fetch: RowFetch,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), WireError> {
        let session = self.session(fetch.terminal).ok_or(WireError {
            code: ErrorCode::UnknownTerminal,
            message: format!("no {}", fetch.terminal),
        })?;
        match fetch_rows(&session, &fetch) {
            Message::Error(error) => Err(error),
            page => write_frame(reply, &page, fetch.generation)
                .await
                .map_err(|e| gone(&e)),
        }
    }

    async fn serve_resource(
        &self,
        fetch: ResourceFetch,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), WireError> {
        let session = self.session(fetch.terminal).ok_or(WireError {
            code: ErrorCode::UnknownTerminal,
            message: format!("no {}", fetch.terminal),
        })?;
        // An iTerm2 image is decoded on the way out, which is no work for
        // the runtime's threads.
        let resource = tokio::task::spawn_blocking(move || fetch_resource(&session, &fetch))
            .await
            .map_err(|e| WireError {
                code: ErrorCode::Limit,
                message: format!("serving an image failed: {e}"),
            })?;
        write_frame(reply, &resource, 0).await.map_err(|e| gone(&e))
    }

    // --- state -------------------------------------------------------------

    fn register(&self, client: ClientInstanceId, principal: &str) -> Result<(), WireError> {
        let mut clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = clients.get(&client) {
            if state.principal != principal {
                // An instance id is not a credential: a different principal
                // presenting a known id is refused rather than inheriting its
                // leases and replies.
                return Err(WireError {
                    code: ErrorCode::Unauthorized,
                    message: "client instance belongs to another principal".to_owned(),
                });
            }
        } else {
            if clients.len() >= MAX_CLIENTS {
                return Err(WireError {
                    code: ErrorCode::Limit,
                    message: format!("{MAX_CLIENTS} clients attached already"),
                });
            }
            clients.insert(
                client,
                ClientState {
                    principal: principal.to_owned(),
                    replies: BTreeMap::new(),
                },
            );
        }
        drop(clients);
        // Back within the grace: the leases are theirs again.
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        for lease in leases.values_mut() {
            if lease.client == client {
                lease.detached_at = None;
            }
        }
        Ok(())
    }

    /// Marks the client's leases as detached and, once the grace has run
    /// out without it coming back, releases them for everyone to see: a
    /// terminal whose controller is gone for good must not keep a
    /// `controller` that stops every other editor from typing into it.
    fn detach(&self, client: ClientInstanceId) {
        let now = Instant::now();
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        for lease in leases.values_mut() {
            if lease.client == client && lease.detached_at.is_none() {
                lease.detached_at = Some(now);
            }
        }
        drop(leases);
        let service = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(LEASE_GRACE).await;
            service.release_expired(client);
        });
    }

    /// Drops every lease `client` still holds detached and past its grace.
    fn release_expired(&self, client: ClientInstanceId) {
        let now = Instant::now();
        let expired: Vec<TerminalId> = {
            let leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
            leases
                .iter()
                .filter(|(_, lease)| lease.client == client && lease.expired(now))
                .map(|(terminal, _)| *terminal)
                .collect()
        };
        for terminal in expired {
            let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
            if leases
                .get(&terminal)
                .is_some_and(|lease| lease.client == client && lease.expired(now))
            {
                leases.remove(&terminal);
                drop(leases);
                if let Some(session) = self.session(terminal) {
                    session.set_controller(None);
                }
            }
        }
    }

    fn attached(
        &self,
        hello: &ClientHello,
        principal: &str,
    ) -> zeughaus_mux::message::ControlAttached {
        let workspace = self.snapshot();
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let heads = workspace
            .terminals()
            .filter_map(|t| sessions.get(&t).map(|s| s.head(0)))
            .collect();
        zeughaus_mux::message::ControlAttached {
            hello: ServerHello {
                major: MAJOR,
                // The lower of the two minors.
                minor: hello.minor.min(MINOR),
                incarnation: self.inner.incarnation,
                capabilities: Vec::<Capability>::new(),
                principal: principal.to_owned(),
                profiles: self
                    .inner
                    .profiles
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (i as u32, p.label.clone()))
                    .collect(),
            },
            workspace,
            heads,
        }
    }

    fn snapshot(&self) -> WorkspaceSnapshot {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = self
            .inner
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let graphs = self.inner.graphs.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = workspace.snapshot(&|t| sessions.get(&t).map(|s| s.title()), &|g| {
            graphs.names.get(&g).cloned()
        });
        // Heads may carry a controller the workspace does not know; the
        // snapshot is structure only. Nothing to merge.
        snapshot.incarnation = self.inner.incarnation;
        snapshot
    }

    /// Makes `session`'s title changes workspace revisions, so a tab named
    /// after the terminal is renamed for every client without any of them
    /// streaming the terminal. Registered before the session is published,
    /// so no title set after the snapshot that first shows it is lost.
    fn follow_title(&self, session: &Session) {
        // Weak: the service owns the session, which owns this listener.
        let inner = Arc::downgrade(&self.inner);
        let terminal = session.id();
        session.on_title_change(move || {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let revision = {
                let mut workspace = inner.workspace.lock().unwrap_or_else(|e| e.into_inner());
                if !workspace.title_changed(terminal) {
                    return;
                }
                workspace.revision()
            };
            inner.publish_revision(revision);
        });
    }

    /// Reports the agent status an omp in `session` sets as the user
    /// variable [`AGENT_VAR`] as activity of its terminal.
    fn follow_agent(&self, session: &Session) {
        // Weak: the service owns the session, which owns this listener.
        let inner = Arc::downgrade(&self.inner);
        let terminal = session.id();
        session.on_user_var(move |name| {
            if name != AGENT_VAR {
                return;
            }
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let service = MuxService { inner };
            let Some(session) = service.session(terminal) else {
                return;
            };
            let agent = agent_status(&session);
            let entry = {
                let metas = service.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
                let Some(meta) = metas.get(&terminal).filter(|meta| !meta.replay) else {
                    return;
                };
                service.entry(terminal, meta, ActivityState::Running, None, agent)
            };
            service.emit(entry);
        });
    }

    /// The session behind a terminal id, if it still exists.
    pub fn session(&self, terminal: TerminalId) -> Option<Session> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&terminal)
            .cloned()
    }

    fn exists(&self, terminal: TerminalId) -> bool {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&terminal)
    }

    fn controls(&self, terminal: TerminalId, client: ClientInstanceId) -> bool {
        self.inner
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&terminal)
            .is_some_and(|lease| lease.client == client)
    }

    /// Whether `client` may drive `terminal`: it holds the lease, or nobody
    /// does, or the holder is gone -- for good, or briefly but under the same
    /// principal (the same user's restarted editor, which a grace meant for
    /// a network blink must not lock out of its own shell).
    fn acquire_if_free(
        &self,
        terminal: TerminalId,
        client: ClientInstanceId,
        principal: &str,
        session: &Session,
    ) -> bool {
        let now = Instant::now();
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        match leases.get(&terminal) {
            Some(lease) if lease.client == client => return true,
            Some(lease) if lease.detached_at.is_some() && lease.principal == principal => {}
            Some(lease) if !lease.expired(now) => return false,
            _ => {}
        }
        let lease = Lease {
            client,
            principal: principal.to_owned(),
            detached_at: None,
        };
        session.set_controller(Some(lease.controller()));
        leases.insert(terminal, lease);
        true
    }

    fn take_control(
        &self,
        terminal: TerminalId,
        client: ClientInstanceId,
        principal: &str,
    ) -> CommandOutcome {
        let Some(session) = self.session(terminal) else {
            return CommandOutcome::Refused {
                reason: format!("no {terminal}"),
            };
        };
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        let lease = Lease {
            client,
            principal: principal.to_owned(),
            detached_at: None,
        };
        session.set_controller(Some(lease.controller()));
        leases.insert(terminal, lease);
        CommandOutcome::Applied {
            revision: *self.inner.revision.borrow(),
        }
    }

    fn release_control(&self, terminal: TerminalId, client: ClientInstanceId) -> CommandOutcome {
        let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
        if leases.get(&terminal).is_some_and(|l| l.client == client) {
            leases.remove(&terminal);
            if let Some(session) = self.session(terminal) {
                session.set_controller(None);
            }
        }
        CommandOutcome::Applied {
            revision: *self.inner.revision.borrow(),
        }
    }

    /// Applies a command once per (client, request id).
    fn command(
        &self,
        client: ClientInstanceId,
        principal: &str,
        command: &Command,
    ) -> CommandOutcome {
        {
            let clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(outcome) = clients
                .get(&client)
                .and_then(|c| c.replies.get(&command.request))
            {
                return outcome.clone();
            }
        }
        let outcome = match &command.command {
            TopologyCommand::TakeControl { terminal } => {
                self.take_control(*terminal, client, principal)
            }
            TopologyCommand::ReleaseControl { terminal } => self.release_control(*terminal, client),
            structural => self.structural(structural),
        };
        let mut clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = clients.get_mut(&client) {
            state.replies.insert(command.request, outcome.clone());
            while state.replies.len() > REMEMBERED_REPLIES {
                state.replies.pop_first();
            }
        }
        outcome
    }

    /// A shell in the work checkout of `origin`, in the runner's project
    /// group of that origin, like a session of it.
    fn open_project(&self, origin: &str, branch: &str) -> CommandOutcome {
        let refused = |reason: String| CommandOutcome::Refused { reason };
        let Some(profile_of) = self.inner.project_profile.get() else {
            return refused("this runner opens no projects".into());
        };
        let profile = match profile_of(origin, branch) {
            Ok(profile) => profile,
            Err(reason) => return refused(reason),
        };
        match self.spawn_owned(
            profile,
            Wal::Capped,
            Dimensions { cols: 80, rows: 24 },
            None,
            OwnedPlacement::Project(origin.to_owned()),
            ActivityKind::Shell,
        ) {
            Ok(_) => CommandOutcome::Applied {
                revision: *self.inner.revision.borrow(),
            },
            Err(reason) => refused(reason),
        }
    }

    fn structural(&self, command: &TopologyCommand) -> CommandOutcome {
        if let TopologyCommand::OpenProject { origin, branch } = command {
            return self.open_project(origin, branch);
        }
        if let TopologyCommand::ReplayRun { run } = command {
            return self.replay(*run);
        }
        let mut spawned: Vec<(TerminalId, Session, TerminalMeta)> = Vec::new();
        let mut spawn = |profile: ProfileId| -> Result<TerminalId, String> {
            let profile = self
                .inner
                .profiles
                .get(profile.0 as usize)
                .ok_or_else(|| format!("no profile {}", profile.0))?;
            let id = TerminalId(self.inner.next_terminal.fetch_add(1, Ordering::Relaxed));
            let mut profile = profile.clone();
            profile
                .env
                .push(("ZEUGHAUS_TERMINAL".to_owned(), id.0.to_string()));
            let session = Session::spawn(
                id,
                &profile,
                Dimensions { cols: 80, rows: 24 },
                Wal::Capped,
                &self.inner.host,
            )
            .map_err(|e| format!("cannot start {}: {e}", profile.label))?;
            let meta = TerminalMeta {
                label: profile.label.clone(),
                scrollback_rows: profile.scrollback_rows,
                run: None,
                kind: ActivityKind::Shell,
                started: now_secs(),
                replay: false,
            };
            spawned.push((id, session, meta));
            Ok(id)
        };
        let result = {
            let mut workspace = self
                .inner
                .workspace
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.known_graph(command).and_then(|()| {
                workspace
                    .apply(command, &mut spawn)
                    .map(|applied| (workspace.revision(), applied))
            })
        };
        match result {
            Ok((revision, applied)) => {
                let mut running = Vec::new();
                let mut forgotten = Vec::new();
                {
                    let mut sessions = self
                        .inner
                        .sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let mut metas = self.inner.meta.lock().unwrap_or_else(|e| e.into_inner());
                    for (id, session, meta) in spawned {
                        self.follow_title(&session);
                        self.follow_agent(&session);
                        running.push(self.entry(id, &meta, ActivityState::Running, None, None));
                        sessions.insert(id, session);
                        metas.insert(id, meta);
                    }
                    let mut leases = self.inner.leases.lock().unwrap_or_else(|e| e.into_inner());
                    for id in &applied.killed {
                        leases.remove(id);
                        forgotten.push((*id, sessions.remove(id), metas.remove(id)));
                    }
                }
                self.inner.publish_revision(revision);
                self.persist();
                for entry in running {
                    self.emit(entry);
                }
                for (id, session, meta) in forgotten {
                    self.forgotten(id, session, meta);
                }
                CommandOutcome::Applied { revision }
            }
            Err(reason) => {
                // A spawn that succeeded before the command failed is a
                // child nobody shows: end it.
                for (_, session, _) in spawned {
                    session.kill();
                }
                CommandOutcome::Refused { reason }
            }
        }
    }

    /// Ends a terminal the workspace let go of: a shell's or session's
    /// output is archived first, and every one but a replay is reported
    /// closed.
    fn forgotten(&self, id: TerminalId, session: Option<Session>, meta: Option<TerminalMeta>) {
        if let Some(meta) = meta.filter(|meta| !meta.replay) {
            let exit = session.as_ref().and_then(Session::exit);
            #[cfg(unix)]
            {
                let mut closed = self.inner.closed.lock().unwrap_or_else(|e| e.into_inner());
                closed.push_back((id, exit.clone()));
                while closed.len() > CLOSED_KEPT {
                    closed.pop_front();
                }
            }
            self.inner
                .gone_agents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(terminal, _)| *terminal != id);
            let mut entry = self.entry(id, &meta, ActivityState::Closed, exit.as_ref(), None);
            if meta.run.is_none() {
                let cols = session.as_ref().map_or(80, |s| s.size().cols);
                entry.run = self.archive(id, &meta, exit.as_ref(), cols);
            }
            self.emit(entry);
        }
        if let Some(session) = session {
            session.kill();
        }
    }

    /// Refuses to open a graph the document does not hold, which would be a
    /// pane with nothing to show. Before the first sync nothing is known and
    /// everything is let through; a sync removes the pane if the graph turns
    /// out to be gone. Called with the workspace locked, as the lock order
    /// asks.
    fn known_graph(&self, command: &TopologyCommand) -> Result<(), String> {
        if let TopologyCommand::OpenGraph { graph } = command
            && self
                .inner
                .graphs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .exists
                .as_ref()
                .is_some_and(|ids| !ids.contains(graph))
        {
            return Err(format!("no graph {graph}"));
        }
        Ok(())
    }
}

/// Unix seconds now.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The agent status the omp in `session` last reported, if it reports one.
fn agent_status(session: &Session) -> Option<AgentStatus> {
    AgentStatus::parse(&session.user_var(AGENT_VAR)?)
}

/// This host's name, as an agent on it reports it.
#[cfg(unix)]
fn local_hostname() -> &'static str {
    static HOST: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let mut buf = [0_u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname writes at
        // most that many bytes.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return String::new();
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).into_owned()
    });
    &HOST
}

/// Whether no process `pid` exists.
#[cfg(unix)]
fn process_gone(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Copies the records of a shim's capped WAL (`wal.1`, then `wal`) into a
/// fresh WAL at `to`. A missing file is skipped.
fn copy_wal(shim_dir: &Path, to: &Path) -> std::io::Result<()> {
    let mut writer = WalWriter::open(to, None)?;
    for name in ["wal.1", "wal"] {
        let records = match wal::read(&shim_dir.join(name)) {
            Ok(records) => records,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for record in records {
            writer.append(record.at_micros, &record.bytes)?;
        }
    }
    Ok(())
}

fn gone(e: &str) -> WireError {
    WireError {
        code: ErrorCode::Protocol,
        message: e.to_owned(),
    }
}

/// The service over real weida exchanges: create a shell, see its output,
/// resize it, let a viewer in, take control, drop every stream and find the
/// child still there on the next attach. Loopback QUIC with mTLS both ways,
/// as the runner binds it.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Run directories nothing is archived into: terminals in this process
    /// have no WAL of their own to keep.
    fn no_runs() -> Arc<RunDirs> {
        Arc::new(RunDirs::new(Path::new("/nonexistent-zeughaus-state"), 1))
    }

    #[test]
    fn a_graph_tab_is_named_after_its_node_and_only_a_known_graph_opens() {
        let service = MuxService::new(
            RunnerIncarnation::from_bytes([7; 16]),
            Vec::new(),
            TerminalHost::Local,
            no_runs(),
        );
        // Before the first sync nothing is known, so nothing is refused.
        assert!(matches!(
            service.structural(&TopologyCommand::OpenGraph { graph: 5 }),
            CommandOutcome::Applied { .. }
        ));
        service.sync_graphs(GraphSync {
            owned: vec![1],
            names: HashMap::from([(1, "Pipeline".to_owned())]),
            exists: HashSet::from([1]),
        });
        let snapshot = service.snapshot();
        assert_eq!(
            snapshot
                .tabs()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["Pipeline"],
            "the missing graph's tab went, the runner's own came"
        );
        assert_eq!(*service.inner.revision.borrow(), snapshot.revision);
        assert!(matches!(
            service.structural(&TopologyCommand::OpenGraph { graph: 5 }),
            CommandOutcome::Refused { .. }
        ));
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use std::time::Duration;

        use tokio::io::AsyncReadExt;
        use weida::{ClientTls, EndpointAddr, Identity, Runtime, ServerTls, SimNet, Trust};
        use zeughaus_mux::input::Key;
        use zeughaus_mux::{
            AttachTarget, Axis, KeyInput, KeyKind, Modifiers, NamedKey, SurfaceRef,
            TerminalAttached, TerminalEvent,
        };

        use zeughaus_terminal::serve::HEAD_ROWS_ABOVE;

        struct Client {
            requester: weida::Requester,
            id: ClientInstanceId,
        }

        struct Exchange {
            request: weida::OutgoingTransfer,
            reply: weida::IncomingTransfer,
        }

        impl Exchange {
            async fn send(&mut self, message: Message, request_id: u64) {
                write_frame(&mut self.request, &message, request_id)
                    .await
                    .expect("write");
            }

            async fn next(&mut self) -> Message {
                tokio::time::timeout(Duration::from_secs(20), read_frame(&mut self.reply))
                    .await
                    .expect("a frame within 20 s")
                    .expect("a readable frame")
                    .1
            }
        }

        async fn open(client: &Client, first: Message) -> Exchange {
            let (mut request, reply) = client
                .requester
                .open(TransferMeta::default())
                .await
                .expect("open");
            write_frame(&mut request, &first, 0)
                .await
                .expect("first frame");
            let reply = reply.recv().await.expect("reply half");
            Exchange { request, reply }
        }

        async fn setup() -> (
            MuxService,
            weida::Runtime,
            weida::Binding,
            String,
            Identity,
            SimNet,
        ) {
            let server_identity = Identity::generate_for(["127.0.0.1"]).expect("identity");
            let client_identity = Identity::generate_for(["client"]).expect("identity");
            let net = SimNet::new();
            let runtime = Runtime::new(crate::transport::sim::config(&net)).expect("runtime");
            let listener = runtime.listener();
            let tls = ServerTls::new(server_identity.clone()).require_client(
                Trust::by_address().and_pin(client_identity.fingerprint().unwrap()),
            );
            let binding = listener
                .bind_quic(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), tls)
                .await
                .expect("bind");
            let replier = listener.replier("/mux").expect("replier");
            let shell = Profile {
                label: "sh".into(),
                program: Some("/bin/sh".into()),
                args: vec![],
                cwd: None,
                env: vec![("PS1".into(), "$ ".into())],
                scrollback_rows: 100,
            };
            let service = MuxService::new(
                RunnerIncarnation::from_bytes([7; 16]),
                vec![shell],
                TerminalHost::Local,
                no_runs(),
            );
            tokio::spawn(service.clone().accept(replier));
            let url = EndpointAddr {
                host: "127.0.0.1".into(),
                port: Some(binding.local_addr().port()),
                path: "/mux".into(),
                peer: Some(server_identity.fingerprint().unwrap()),
            }
            .to_string();
            (service, runtime, binding, url, client_identity, net)
        }

        async fn client(runtime: &Runtime, url: &str, identity: &Identity, seed: u8) -> Client {
            let requester = runtime
                .requester(ClientTls::new(Trust::by_address()).with_identity(identity.clone()));
            requester.connect(url).await.expect("connect");
            Client {
                requester,
                id: ClientInstanceId::from_bytes([seed; 16]),
            }
        }

        fn hello(client: &Client) -> Message {
            Message::ControlAttach(ControlAttach {
                hello: ClientHello {
                    major: MAJOR,
                    minor: MINOR,
                    client: client.id,
                    capabilities: vec![],
                    known_incarnation: None,
                    known_revision: None,
                },
            })
        }

        fn row_text(rows: &[zeughaus_mux::RowData]) -> String {
            rows.iter()
                .flat_map(|r| r.spans.iter().map(|s| s.text.as_str()))
                .collect::<Vec<_>>()
                .join("|")
        }

        /// Waits for deltas until `pred` holds on one, collecting events.
        async fn until(
            exchange: &mut Exchange,
            mut pred: impl FnMut(&zeughaus_mux::TerminalDelta) -> bool,
        ) -> Vec<TerminalEvent> {
            let mut events = Vec::new();
            for _ in 0..200 {
                match exchange.next().await {
                    Message::TerminalDelta(delta) => {
                        events.extend(delta.ordered_events.iter().cloned());
                        if pred(&delta) {
                            return events;
                        }
                    }
                    other => panic!("unexpected {:?}", other.kind()),
                }
            }
            panic!("no delta satisfied the predicate");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_shell_is_created_driven_shared_and_survives_its_viewers() {
            let (service, runtime, _binding, url, identity, _net) = setup().await;
            let alice = client(&runtime, &url, &identity, 1).await;

            // Attach: the default workspace, no terminals.
            let mut control = open(&alice, hello(&alice)).await;
            let Message::ControlAttached(attached) = control.next().await else {
                panic!("expected ControlAttached");
            };
            assert_eq!(attached.workspace.tabs().count(), 0);
            assert!(attached.heads.is_empty());
            assert_eq!(
                attached.hello.principal,
                identity.fingerprint().unwrap().to_string()
            );

            // A new terminal tab: reply, then the snapshot that shows it.
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::NewTerminalTab {
                            profile: ProfileId::DEFAULT,
                        },
                    }),
                    1,
                )
                .await;
            let mut terminal = None;
            for _ in 0..2 {
                match control.next().await {
                    Message::CommandReply(reply) => {
                        assert_eq!(reply.request, RequestId(1));
                        assert!(matches!(
                            reply.outcome,
                            CommandOutcome::Applied { revision: 2 }
                        ));
                    }
                    Message::WorkspaceSnapshot(snapshot) => {
                        assert_eq!(snapshot.revision, 2);
                        terminal = snapshot.terminals().next();
                    }
                    other => panic!("unexpected {:?}", other.kind()),
                }
            }
            let terminal = terminal.expect("the snapshot names the terminal");

            // The same request id again is answered, not applied again.
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::NewTerminalTab {
                            profile: ProfileId::DEFAULT,
                        },
                    }),
                    1,
                )
                .await;
            assert!(matches!(
                control.next().await,
                Message::CommandReply(CommandReply {
                    outcome: CommandOutcome::Applied { revision: 2 },
                    ..
                })
            ));

            // Attach to the terminal, type, see the echo.
            let mut term = open(
                &alice,
                Message::TerminalAttach(TerminalAttach {
                    client: alice.id,
                    terminal,
                    known: None,
                    size: Dimensions { cols: 40, rows: 10 },
                }),
            )
            .await;
            let Message::TerminalAttached(TerminalAttached { head: Some(head) }) =
                term.next().await
            else {
                panic!("expected a head");
            };
            assert_eq!(head.terminal, terminal);
            term.send(
                Message::TerminalCommand(TerminalCommand::Text {
                    serial: 1,
                    text: "echo mux-ok\n".into(),
                }),
                0,
            )
            .await;
            let mut seen = String::new();
            until(&mut term, |delta| {
                seen.push_str(&row_text(&delta.row_replacements));
                seen.contains("mux-ok") && delta.input_serial_ack >= 1
            })
            .await;
            assert!(
                service.controls(terminal, alice.id),
                "the first typist holds the lease"
            );

            // Resize as the controller.
            term.send(
                Message::TerminalCommand(TerminalCommand::Resize {
                    size: Dimensions { cols: 60, rows: 12 },
                    cell: zeughaus_mux::CellPixels::NOMINAL,
                }),
                0,
            )
            .await;
            until(&mut term, |delta| {
                delta.dimensions == Some(Dimensions { cols: 60, rows: 12 })
            })
            .await;

            // A viewer: sees the screen, cannot type, can take control.
            let bob = client(&runtime, &url, &identity, 2).await;
            let mut bob_control = open(&bob, hello(&bob)).await;
            let Message::ControlAttached(attached) = bob_control.next().await else {
                panic!("expected ControlAttached");
            };
            assert_eq!(attached.heads.len(), 1);
            assert!(row_text(&attached.heads[0].rows).contains("mux-ok"));
            assert_eq!(
                attached.heads[0].controller.as_ref().map(|c| c.client),
                Some(alice.id)
            );
            let mut bob_term = open(
                &bob,
                Message::TerminalAttach(TerminalAttach {
                    client: bob.id,
                    terminal,
                    known: None,
                    size: Dimensions { cols: 60, rows: 12 },
                }),
            )
            .await;
            let Message::TerminalAttached(TerminalAttached { head: Some(_) }) =
                bob_term.next().await
            else {
                panic!("expected a head");
            };
            bob_term
                .send(
                    Message::TerminalCommand(TerminalCommand::Text {
                        serial: 1,
                        text: "echo viewer-typed\n".into(),
                    }),
                    0,
                )
                .await;
            // Nothing of Bob's reaches the shell: prove it by typing as Alice
            // afterwards and checking what arrived first.
            term.send(
                Message::TerminalCommand(TerminalCommand::Text {
                    serial: 2,
                    text: "echo alice-again\n".into(),
                }),
                0,
            )
            .await;
            let mut seen = String::new();
            until(&mut term, |delta| {
                seen.push_str(&row_text(&delta.row_replacements));
                seen.contains("alice-again")
            })
            .await;
            assert!(
                !seen.contains("viewer-typed"),
                "a viewer's input is dropped: {seen}"
            );

            // Semantic keys, not text: what a widget sends per keystroke.
            let mut serial = 3;
            for ch in "echo key-ok".chars() {
                term.send(
                    Message::TerminalCommand(TerminalCommand::Key {
                        serial,
                        input: KeyInput {
                            key: Key::Char(ch),
                            modifiers: Modifiers::default(),
                            kind: KeyKind::Press,
                        },
                    }),
                    0,
                )
                .await;
                serial += 1;
            }
            term.send(
                Message::TerminalCommand(TerminalCommand::Key {
                    serial,
                    input: KeyInput {
                        key: Key::Named(NamedKey::Enter),
                        modifiers: Modifiers::default(),
                        kind: KeyKind::Press,
                    },
                }),
                0,
            )
            .await;
            let mut seen = String::new();
            until(&mut term, |delta| {
                seen.push_str(&row_text(&delta.row_replacements));
                seen.contains("key-ok") && delta.input_serial_ack >= serial
            })
            .await;

            // A burst that scrolls: rows that left the screen between two deltas
            // were never sent, and a fetch is how a viewer gets them.
            term.send(
                Message::TerminalCommand(TerminalCommand::Text {
                    serial: serial + 1,
                    text: "seq 1 100\n".into(),
                }),
                0,
            )
            .await;
            // The echo of the command line holds "100" too, before anything
            // scrolled; wait until the screen has moved past the fetched range.
            let mut visible = None;
            until(&mut term, |delta| {
                if let Some(v) = delta.visible {
                    visible = Some(v);
                }
                visible.is_some_and(|v| v.start >= 40)
            })
            .await;
            let visible = visible.expect("a delta names the visible rows");
            let mut fetch = open(
                &alice,
                Message::RowFetch(RowFetch {
                    terminal,
                    epoch: 1,
                    range: zeughaus_mux::StableRange {
                        start: (visible.start - 40).max(0),
                        end: visible.start,
                    },
                    generation: 7,
                }),
            )
            .await;
            let Message::RowPage(page) = fetch.next().await else {
                panic!("expected a page");
            };
            assert_eq!(page.generation, 7);
            assert!(!page.rows.is_empty(), "the rows above the screen exist");
            let text = row_text(&page.rows);
            assert!(
                text.contains("|5|") || text.contains("50"),
                "the page holds the burst's earlier lines: {text}"
            );

            bob_control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::TakeControl { terminal },
                    }),
                    1,
                )
                .await;
            assert!(matches!(bob_control.next().await, Message::CommandReply(_)));
            let events = until(&mut term, |delta| {
                delta.ordered_events.iter().any(
                |e| matches!(e, TerminalEvent::ControllerChanged(Some(c)) if c.client == bob.id),
            )
            })
            .await;
            assert!(!events.is_empty());
            assert!(service.controls(terminal, bob.id));

            // Everyone leaves; the child keeps running; the next attach finds
            // the screen as it was.
            drop(term);
            drop(bob_term);
            drop(control);
            drop(bob_control);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let session = service
                .session(terminal)
                .expect("the session outlives its viewers");
            assert!(
                session.exit().is_none(),
                "the child was not killed by a detach"
            );

            let carol = client(&runtime, &url, &identity, 3).await;
            let mut control = open(&carol, hello(&carol)).await;
            let Message::ControlAttached(attached) = control.next().await else {
                panic!("expected ControlAttached");
            };
            assert!(row_text(&attached.heads[0].rows).contains("100"));
            // Bob's lease is within its grace, and Carol proves the same identity
            // Bob did -- the same user's restarted editor. Her first keystroke
            // takes the shell over rather than waiting the grace out.
            assert!(service.controls(terminal, bob.id));
            let mut carol_term = open(
                &carol,
                Message::TerminalAttach(TerminalAttach {
                    client: carol.id,
                    terminal,
                    known: None,
                    size: Dimensions { cols: 60, rows: 12 },
                }),
            )
            .await;
            let _ = carol_term.next().await;
            carol_term
                .send(
                    Message::TerminalCommand(TerminalCommand::Text {
                        serial: 1,
                        text: "echo carol-back\n".into(),
                    }),
                    0,
                )
                .await;
            let mut seen = String::new();
            until(&mut carol_term, |delta| {
                seen.push_str(&row_text(&delta.row_replacements));
                seen.contains("carol-back")
            })
            .await;
            assert!(service.controls(terminal, carol.id));
            drop(carol_term);

            // Closing the pane kills the child and leaves no tab behind.
            let pane = attached.workspace.tabs().next().unwrap().root.leaves()[0].0;
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::ClosePane { pane },
                    }),
                    1,
                )
                .await;
            for _ in 0..2 {
                match control.next().await {
                    Message::CommandReply(reply) => {
                        assert!(matches!(reply.outcome, CommandOutcome::Applied { .. }));
                    }
                    Message::WorkspaceSnapshot(snapshot) => {
                        assert_eq!(snapshot.tabs().count(), 0);
                    }
                    other => panic!("unexpected {:?}", other.kind()),
                }
            }
            for _ in 0..50 {
                if session.exit().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(matches!(
                session.exit(),
                Some(zeughaus_mux::ExitState::Killed)
            ));
            assert!(service.session(terminal).is_none());

            // A stranger's exchange kind, and a key that means nothing to the
            // graph: both refused in words, never a panic.
            let mut bad = open(
                &carol,
                Message::TerminalCommand(TerminalCommand::Key {
                    serial: 1,
                    input: KeyInput {
                        key: Key::Named(NamedKey::Enter),
                        modifiers: Modifiers::default(),
                        kind: KeyKind::Press,
                    },
                }),
            )
            .await;
            assert!(matches!(
                bad.next().await,
                Message::Error(WireError {
                    code: ErrorCode::Protocol,
                    ..
                })
            ));
            let mut buf = [0u8; 1];
            assert_eq!(
                bad.reply.read(&mut buf).await.ok(),
                Some(0),
                "the exchange is finished"
            );

            // A graph opened on the reconnected control is a pane like any other.
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(2),
                        command: TopologyCommand::OpenGraph { graph: 7 },
                    }),
                    2,
                )
                .await;
            let mut graph_pane = None;
            for _ in 0..2 {
                if let Message::WorkspaceSnapshot(s) = control.next().await {
                    graph_pane = s
                        .tabs()
                        .flat_map(|tab| tab.root.leaves())
                        .find(|(_, surface)| *surface == SurfaceRef::Graph(7))
                        .map(|(pane, _)| pane);
                }
            }
            let graph_pane = graph_pane.expect("the snapshot shows the graph");
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(3),
                        command: TopologyCommand::SplitWithTerminal {
                            pane: graph_pane,
                            axis: Axis::Horizontal,
                            profile: ProfileId::DEFAULT,
                        },
                    }),
                    3,
                )
                .await;
            let mut split_seen = false;
            for _ in 0..2 {
                if let Message::WorkspaceSnapshot(s) = control.next().await {
                    split_seen = s.tabs().next().unwrap().root.leaf_count() == 2;
                }
            }
            assert!(split_seen);
        }

        /// The plan's acceptance numbers, measured on loopback: a fresh dial plus
        /// attach (QUIC, TLS, HELLO, one control exchange), a warm attach on the
        /// pooled connection, and a keystroke's round trip to the delta that
        /// echoes it. Ignored in the gate: it is a measurement, not a contract,
        /// and it is meant for `cargo test --release -- --ignored perf`.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore]
        async fn perf_attach_and_keystroke_latency() {
            fn percentiles(samples: &mut [Duration]) -> (Duration, Duration) {
                samples.sort();
                let p50 = samples[samples.len() / 2];
                let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
                (p50, p95)
            }

            let (_service, runtime, _binding, url, identity, net) = setup().await;

            // Fresh dial: a new requester each time, so the pool cannot reuse.
            let mut fresh = Vec::new();
            for i in 0..20u8 {
                // A runtime of its own per dial: the pool keys on the trust
                // configuration, and an equal one would hand back the warm
                // connection.
                let fresh_runtime =
                    Runtime::new(crate::transport::sim::config(&net)).expect("runtime");
                let started = Instant::now();
                let client = client(&fresh_runtime, &url, &identity, 10 + i).await;
                let mut control = open(&client, hello(&client)).await;
                let Message::ControlAttached(_) = control.next().await else {
                    panic!("expected ControlAttached");
                };
                fresh.push(started.elapsed());
            }
            let (p50, p95) = percentiles(&mut fresh);
            eprintln!("fresh dial + attach: p50 {p50:?} p95 {p95:?}");

            // Warm attach: one requester, one pooled connection, many exchanges.
            let alice = client(&runtime, &url, &identity, 1).await;
            let mut warm = Vec::new();
            for _ in 0..50 {
                let started = Instant::now();
                let mut control = open(&alice, hello(&alice)).await;
                let Message::ControlAttached(_) = control.next().await else {
                    panic!("expected ControlAttached");
                };
                warm.push(started.elapsed());
            }
            let (p50, p95) = percentiles(&mut warm);
            eprintln!("warm attach: p50 {p50:?} p95 {p95:?}");

            // A terminal, then keystrokes: the time from sending a key to the
            // delta that shows its echo.
            let mut control = open(&alice, hello(&alice)).await;
            let _ = control.next().await;
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::NewTerminalTab {
                            profile: ProfileId::DEFAULT,
                        },
                    }),
                    1,
                )
                .await;
            let mut terminal = None;
            for _ in 0..2 {
                if let Message::WorkspaceSnapshot(s) = control.next().await {
                    terminal = s.terminals().next();
                }
            }
            let terminal = terminal.expect("terminal");
            let mut term = open(
                &alice,
                Message::TerminalAttach(TerminalAttach {
                    client: alice.id,
                    terminal,
                    known: None,
                    size: Dimensions { cols: 80, rows: 24 },
                }),
            )
            .await;
            let _ = term.next().await;
            // Let the shell print its prompt before timing anything.
            tokio::time::sleep(Duration::from_millis(300)).await;
            while tokio::time::timeout(Duration::from_millis(200), term.next())
                .await
                .is_ok()
            {}

            let mut keys = Vec::new();
            for i in 0..50u64 {
                let ch = char::from(b'a' + (i % 26) as u8);
                // A person types with gaps well past the coalescing window;
                // back-to-back keys would measure the burst pacing instead.
                tokio::time::sleep(Duration::from_millis(40)).await;
                let started = Instant::now();
                term.send(
                    Message::TerminalCommand(TerminalCommand::Key {
                        serial: i + 1,
                        input: KeyInput {
                            key: Key::Char(ch),
                            modifiers: Modifiers::default(),
                            kind: KeyKind::Press,
                        },
                    }),
                    0,
                )
                .await;
                until(&mut term, |delta| delta.input_serial_ack > i).await;
                keys.push(started.elapsed());
            }
            let (p50, p95) = percentiles(&mut keys);
            eprintln!("keystroke to echoed delta: p50 {p50:?} p95 {p95:?}");

            // Codec cost of what an attach carries.
            let head = _service
                .session(terminal)
                .expect("session")
                .head(HEAD_ROWS_ABOVE);
            let started = Instant::now();
            let bytes = Message::TerminalHead(head.clone())
                .encode(0)
                .expect("encode");
            let encode = started.elapsed();
            let started = Instant::now();
            let header = zeughaus_mux::FrameHeader::decode(
                bytes[..zeughaus_mux::FrameHeader::LEN].try_into().unwrap(),
            )
            .unwrap();
            let _ = Message::decode(header.kind, &bytes[zeughaus_mux::FrameHeader::LEN..]).unwrap();
            let decode = started.elapsed();
            eprintln!(
                "head of {} rows: {} bytes, encode {encode:?}, decode {decode:?}",
                head.rows.len(),
                bytes.len()
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_terminal_the_runner_owns_is_listed_attached_and_only_closed_on_demand() {
            let (service, runtime, _binding, url, identity, _net) = setup().await;
            let job = service
                .spawn_owned(
                    Profile {
                        label: "job".into(),
                        program: Some("/bin/sh".into()),
                        args: vec!["-c".into(), "sleep 30".into()],
                        cwd: None,
                        env: vec![],
                        scrollback_rows: 100,
                    },
                    Wal::Off,
                    Dimensions { cols: 80, rows: 24 },
                    None,
                    OwnedPlacement::Detached,
                    ActivityKind::Job,
                )
                .expect("start the job's process");

            let alice = client(&runtime, &url, &identity, 1).await;
            let mut control = open(&alice, hello(&alice)).await;
            let Message::ControlAttached(attached) = control.next().await else {
                panic!("expected ControlAttached");
            };
            assert_eq!(
                attached
                    .workspace
                    .detached
                    .iter()
                    .map(|d| d.terminal)
                    .collect::<Vec<_>>(),
                vec![job],
                "an owned terminal is listed before any pane shows it"
            );
            assert_eq!(attached.workspace.terminals().count(), 0);

            control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::AttachTerminal {
                            terminal: job,
                            target: AttachTarget::NewTab,
                        },
                    }),
                    1,
                )
                .await;
            let mut shown = false;
            for _ in 0..2 {
                match control.next().await {
                    Message::CommandReply(reply) => {
                        assert!(matches!(reply.outcome, CommandOutcome::Applied { .. }));
                    }
                    Message::WorkspaceSnapshot(snapshot) => {
                        assert!(snapshot.detached.is_empty());
                        assert_eq!(snapshot.terminals().collect::<Vec<_>>(), vec![job]);
                        shown = true;
                    }
                    other => panic!("unexpected {:?}", other.kind()),
                }
            }
            assert!(shown, "the snapshot after an attach shows the terminal");

            service.close_terminal(job).expect("close the job terminal");
            assert!(
                service.session(job).is_none(),
                "a closed terminal is forgotten"
            );
            assert!(service.close_terminal(job).is_err());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_title_the_child_sets_renames_its_tab_without_a_terminal_stream() {
            let (service, runtime, _binding, url, identity, _net) = setup().await;
            let alice = client(&runtime, &url, &identity, 1).await;
            let mut control = open(&alice, hello(&alice)).await;
            let Message::ControlAttached(_) = control.next().await else {
                panic!("expected ControlAttached");
            };
            control
                .send(
                    Message::Command(Command {
                        request: RequestId(1),
                        command: TopologyCommand::NewTerminalTab {
                            profile: ProfileId::DEFAULT,
                        },
                    }),
                    1,
                )
                .await;
            let mut terminal = None;
            for _ in 0..2 {
                if let Message::WorkspaceSnapshot(snapshot) = control.next().await {
                    terminal = snapshot.terminals().next();
                }
            }
            let terminal = terminal.expect("the snapshot names the terminal");

            // Nobody attaches to the terminal: the title has to arrive on the
            // control exchange alone.
            service
                .session(terminal)
                .expect("session")
                .apply(&TerminalCommand::Text {
                    serial: 1,
                    text: "printf '\\033]0;renamed\\007'\n".into(),
                })
                .expect("type");
            for _ in 0..20 {
                match control.next().await {
                    Message::WorkspaceSnapshot(snapshot)
                        if snapshot
                            .tabs()
                            .next()
                            .is_some_and(|tab| tab.title == "renamed") =>
                    {
                        let tab = snapshot.tabs().next().unwrap().id;
                        assert!(matches!(
                            service.structural(&TopologyCommand::CloseTab { tab }),
                            CommandOutcome::Applied { .. }
                        ));
                        return;
                    }
                    Message::WorkspaceSnapshot(_) => {}
                    other => panic!("unexpected {:?}", other.kind()),
                }
            }
            panic!("no snapshot carried the new title");
        }
    }
}
