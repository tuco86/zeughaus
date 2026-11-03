//! Agent sessions the CI asks this runner to open, each in a terminal of the
//! mux, and what this runner tells the CI about them.
//!
//! `<state-dir>/sessions.json` records each session the CI started here with
//! its terminal, so a restarted runner still reports and watches it. A
//! watcher per session reports its end as `SessionEnded` and drops the
//! record; an end that could not be sent while the CI was unreachable stays
//! recorded and goes out with the next `Hello`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeughaus_link::enlist::{FromRunner, SessionReport, SessionSpec};
use zeughaus_mux::ExitState;

use super::{Executor, lock};

/// How often a watcher looks at its session's terminal.
const WATCH_EVERY: Duration = Duration::from_secs(1);

/// What opens and closes a session's terminal: the runner's agents, on a
/// runner with a terminal mux.
pub trait SessionHost: Send + Sync {
    /// The terminal the session runs in.
    fn open(&self, spec: &SessionSpec) -> Result<u64, String>;
    fn close(&self, terminal: u64) -> Result<(), String>;
    /// The terminal runs (`Some(None)`), its child ended (`Some(Some(exit))`),
    /// or it is gone without an exit (`None`).
    fn state(&self, terminal: u64) -> Option<Option<ExitState>>;
    /// Whether a session's terminal outlives this process (unix shims); a
    /// terminal without one ends with the runner.
    fn keeps_terminals(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    id: u64,
    terminal: u64,
    /// `(code, killed)` once it ended and the CI was not told yet.
    #[serde(default)]
    end: Option<(Option<i32>, bool)>,
}

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions.json")
}

fn load(state_dir: &Path) -> Vec<Record> {
    std::fs::read(path(state_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store(state_dir: &Path, records: &[Record]) {
    if let Err(e) = crate::files::write_json(&path(state_dir), &records) {
        eprintln!("[enlist] sessions: {e}");
    }
}

/// `(code, killed)` of how a terminal's child ended.
fn ending(exit: &ExitState) -> (Option<i32>, bool) {
    match exit {
        ExitState::Exited { code } => (Some(i32::try_from(*code).unwrap_or(-1)), false),
        ExitState::Killed => (None, true),
        _ => (None, false),
    }
}

impl Executor {
    /// Changes the records under the sessions lock.
    fn change_sessions<T>(&self, change: impl FnOnce(&mut Vec<Record>) -> T) -> T {
        let _held = lock(&self.sessions_lock);
        let mut records = load(&self.state_dir);
        let answer = change(&mut records);
        store(&self.state_dir, &records);
        answer
    }

    /// Opens session `spec` on a thread of its own and answers the CI.
    pub(super) fn open_session(self: &Arc<Self>, spec: SessionSpec) {
        let exec = Arc::clone(self);
        std::thread::spawn(move || {
            let id = spec.id;
            let Some(host) = exec.agents.clone() else {
                exec.send(FromRunner::SessionFailed {
                    id,
                    message: "agent sessions need a runner with a terminal mux".to_owned(),
                });
                return;
            };
            match host.open(&spec) {
                Ok(terminal) => {
                    eprintln!("[enlist] session {id} in terminal {terminal}");
                    exec.change_sessions(|records| {
                        records.retain(|r| r.id != id);
                        records.push(Record {
                            id,
                            terminal,
                            end: None,
                        });
                    });
                    exec.send(FromRunner::SessionOpened { id, terminal });
                    exec.watch_session(id, terminal);
                }
                Err(message) => {
                    eprintln!("[enlist] session {id} refused: {message}");
                    exec.send(FromRunner::SessionFailed { id, message });
                }
            }
        });
    }

    /// Ends session `id`'s terminal; its watcher reports the end.
    pub(super) fn close_session(&self, id: u64) {
        let record = {
            let _held = lock(&self.sessions_lock);
            load(&self.state_dir).into_iter().find(|r| r.id == id)
        };
        let (Some(record), Some(host)) = (record, self.agents.as_ref()) else {
            eprintln!("[enlist] no session {id} to close");
            return;
        };
        if let Err(e) = host.close(record.terminal) {
            eprintln!("[enlist] cannot close session {id}: {e}");
        }
    }

    /// Watches the terminal of session `id` on this thread until it ends.
    fn watch_session(self: &Arc<Self>, id: u64, terminal: u64) {
        let Some(host) = self.agents.clone() else {
            return;
        };
        let end = loop {
            match host.state(terminal) {
                Some(None) => std::thread::sleep(WATCH_EVERY),
                Some(Some(exit)) => break ending(&exit),
                None => break (None, true),
            }
        };
        eprintln!("[enlist] session {id} ended");
        let sent = self.send(FromRunner::SessionEnded {
            id,
            code: end.0,
            killed: end.1,
        });
        self.change_sessions(|records| {
            if sent {
                records.retain(|r| r.id != id);
            } else if let Some(record) = records.iter_mut().find(|r| r.id == id) {
                record.end = Some(end);
            }
        });
    }

    /// Watches the sessions a previous process recorded and did not see end.
    pub(super) fn recover_sessions(self: &Arc<Self>) {
        let Some(host) = self.agents.as_ref() else {
            return;
        };
        // Terminals without shims ended with the process that recorded them;
        // the ids are handed out again from 1, so they must not be watched.
        if !host.keeps_terminals() {
            self.change_sessions(|records| {
                for record in records.iter_mut().filter(|r| r.end.is_none()) {
                    record.end = Some((None, true));
                }
            });
            return;
        }
        for record in load(&self.state_dir) {
            if record.end.is_none() {
                let exec = Arc::clone(self);
                std::thread::spawn(move || exec.watch_session(record.id, record.terminal));
            }
        }
    }

    /// The sessions this runner holds, for `Hello`.
    pub(super) fn session_reports(&self) -> Vec<SessionReport> {
        let _held = lock(&self.sessions_lock);
        load(&self.state_dir)
            .into_iter()
            .map(|r| SessionReport {
                id: r.id,
                terminal: r.terminal,
                end: r.end,
            })
            .collect()
    }

    /// After a welcome: the ends `Hello` carried are told.
    pub(super) fn sessions_reported(&self) {
        self.change_sessions(|records| records.retain(|r| r.end.is_none()));
    }
}
