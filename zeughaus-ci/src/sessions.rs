//! Agent sessions: the registry of the sessions the CI started on its
//! runners, and the dispatcher that starts a master session for each
//! approved plan on a free runner its `Runs on` matches.
//!
//! A master session executes one plan and hosts nothing else: a runner runs
//! at most one master at a time, and the runners a master reaches over ssh
//! are reserved for it too, so several plans run at once on different
//! runners. Plan sessions (omp in plan mode) are opened on request, never
//! count as busy and reserve nothing.
//!
//! The registry is `<state-dir>/ci/sessions.json`: every live session and
//! the newest [`KEPT_ENDED`] ended ones. A runner reports what became of its
//! sessions (`SessionOpened`, `SessionFailed`, `SessionEnded`) and, when it
//! enlists again, the sessions it still holds; a live session it no longer
//! knows ended with it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use zeughaus_link::enlist::{FromRunner, SessionReport, SessionSpec, ToRunner};
use zeughaus_link::selector::{Selector, parse_runs_on};
use zeughaus_link::{
    DocKind, DocState, SessionKind, SessionMachine, SessionState, SessionView, normalize_origin,
};

use crate::config::CiConfig;
use crate::docs;
use crate::files::{read_json, write_json};
use crate::runners::{RunnerView, Runners};

/// Ended sessions the registry keeps.
const KEPT_ENDED: usize = 100;

/// Ended sessions the overview lists.
const LISTED_ENDED: usize = 20;

/// How long a runner on which a master failed to start gets no other.
const COOLDOWN_SECS: u64 = 300;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    next: u64,
    list: Vec<SessionView>,
}

pub struct Sessions {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    runners: Arc<Runners>,
    /// The documents' lock, shared with the `/ci` server.
    docs_lock: Arc<Mutex<()>>,
    /// The repository of each document change, announced as `CiDocs`.
    docs_tx: Sender<String>,
    /// One message per change of the registry, announced as `CiSessions`.
    events_tx: Sender<()>,
    book: Mutex<Book>,
    /// Runner -> the time before which it gets no master.
    cooldown: Mutex<BTreeMap<String, u64>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn book_path(state_dir: &Path) -> PathBuf {
    crate::ci_dir(state_dir).join("sessions.json")
}

/// Where the session's host and its other machines are: the first selector
/// picks a host from `hosts`, each further one a distinct runner from
/// `extras`, in that order. Both lists are `(name, labels)` in name order,
/// which breaks ties. `None` when a selector finds nothing.
pub fn place(
    selectors: &[Selector],
    hosts: &[(String, Vec<String>)],
    extras: &[(String, Vec<String>)],
) -> Option<Vec<String>> {
    let (first, rest) = selectors.split_first()?;
    let host = hosts
        .iter()
        .find(|(_, labels)| first.matches(labels))?
        .0
        .clone();
    let mut chosen = vec![host];
    for selector in rest {
        let extra = extras
            .iter()
            .find(|(name, labels)| !chosen.contains(name) && selector.matches(labels))?;
        chosen.push(extra.0.clone());
    }
    Some(chosen)
}

impl Sessions {
    pub fn new(
        state_dir: PathBuf,
        config: Arc<CiConfig>,
        runners: Arc<Runners>,
        docs_lock: Arc<Mutex<()>>,
        docs_tx: Sender<String>,
        events_tx: Sender<()>,
    ) -> Sessions {
        let book = read_json::<Book>(&book_path(&state_dir)).unwrap_or_default();
        Sessions {
            state_dir,
            config,
            runners,
            docs_lock,
            docs_tx,
            events_tx,
            book: Mutex::new(book),
            cooldown: Mutex::new(BTreeMap::new()),
        }
    }

    /// Keeps the live sessions and the newest ended ones, writes the book
    /// and announces the change.
    fn changed(&self, book: &mut Book) {
        let mut ended: Vec<u64> = book
            .list
            .iter()
            .filter(|s| !s.state.is_live())
            .map(|s| s.id)
            .collect();
        ended.sort_unstable_by(|a, b| b.cmp(a));
        let dropped: BTreeSet<u64> = ended.into_iter().skip(KEPT_ENDED).collect();
        book.list.retain(|s| !dropped.contains(&s.id));
        if let Err(e) = write_json(&book_path(&self.state_dir), &*book) {
            eprintln!("[ci] sessions: {e}");
        }
        let _ = self.events_tx.send(());
    }

    /// The live sessions, then the newest ended ones.
    pub fn listed(&self) -> Vec<SessionView> {
        let book = lock(&self.book);
        let mut live: Vec<SessionView> = book
            .list
            .iter()
            .filter(|s| s.state.is_live())
            .cloned()
            .collect();
        let mut ended: Vec<SessionView> = book
            .list
            .iter()
            .filter(|s| !s.state.is_live())
            .cloned()
            .collect();
        ended.sort_by_key(|s| std::cmp::Reverse(s.id));
        ended.truncate(LISTED_ENDED);
        live.sort_by_key(|s| s.id);
        live.extend(ended);
        live
    }

    pub fn session(&self, id: u64) -> Option<SessionView> {
        lock(&self.book).list.iter().find(|s| s.id == id).cloned()
    }

    /// The runners that host or are reserved for a live master.
    fn busy(book: &Book) -> BTreeSet<String> {
        book.list
            .iter()
            .filter(|s| s.kind == SessionKind::Master && s.state.is_live())
            .flat_map(|s| {
                std::iter::once(s.runner.clone()).chain(s.machines.iter().map(|m| m.name.clone()))
            })
            .collect()
    }

    /// The connected runners that may host sessions, with their labels.
    fn session_hosts(&self, views: &BTreeMap<String, RunnerView>) -> Vec<(String, Vec<String>)> {
        self.config
            .runners
            .iter()
            .filter(|(_, runner)| runner.sessions)
            .filter_map(|(name, _)| {
                let view = views.get(name)?;
                Some((name.clone(), self.config.labels_of(name, Some(&view.offer))))
            })
            .collect()
    }

    /// What a session needs to know of runner `name` as one of its machines.
    fn machine(&self, name: &str, views: &BTreeMap<String, RunnerView>) -> SessionMachine {
        let view = views.get(name);
        SessionMachine {
            name: name.to_owned(),
            ssh: self.config.runners.get(name).and_then(|r| r.ssh.clone()),
            labels: self.config.labels_of(name, view.map(|v| &v.offer)),
            addresses: view.map(|v| v.addresses.clone()).unwrap_or_default(),
        }
    }

    /// What the runner of `view` is sent to open it.
    fn spec(&self, view: &SessionView) -> Option<SessionSpec> {
        let repo = self.config.repos.get(&view.repo)?;
        Some(SessionSpec {
            id: view.id,
            kind: view.kind,
            repo: view.repo.clone(),
            origin: normalize_origin(&repo.url),
            default_branch: repo.default_branch.clone(),
            doc: view.doc,
            runner: view.runner.clone(),
            machines: view.machines.clone(),
            ci: zeughaus_link::credentials::read_endpoint(&self.state_dir).unwrap_or_default(),
        })
    }

    /// Starts a master for each approved plan that finds free runners, in
    /// queue order. A plan that finds none stays queued; a later one may
    /// still find its own.
    pub fn dispatch(&self, now: u64) {
        let queue = docs::queue(&self.state_dir, &self.config);
        if queue.is_empty() {
            return;
        }
        let views = self.runners.views();
        for plan in queue {
            if docs::paused(&self.state_dir, &self.config, &plan.repo) {
                continue;
            }
            let Ok(selectors) = parse_runs_on(&plan.runs_on) else {
                continue;
            };
            let mut book = lock(&self.book);
            let busy = Self::busy(&book);
            let cooldown = lock(&self.cooldown).clone();
            let free = |name: &String| !busy.contains(name);
            let hosts: Vec<(String, Vec<String>)> = self
                .session_hosts(&views)
                .into_iter()
                .filter(|(name, _)| free(name) && cooldown.get(name).is_none_or(|&t| t <= now))
                .collect();
            let extras: Vec<(String, Vec<String>)> = self
                .config
                .runners
                .iter()
                .filter(|(name, runner)| runner.ssh.is_some() && free(name))
                .filter_map(|(name, _)| {
                    let view = views.get(name)?;
                    Some((name.clone(), self.config.labels_of(name, Some(&view.offer))))
                })
                .collect();
            let Some(placement) = place(&selectors, &hosts, &extras) else {
                continue;
            };
            let started = {
                let _held = lock(&self.docs_lock);
                docs::start(
                    &self.state_dir,
                    &self.config,
                    &plan.repo,
                    plan.number,
                    placement.clone(),
                    now,
                )
            };
            if let Err(e) = started {
                eprintln!("[ci] sessions: {}#{}: {e}", plan.repo, plan.number);
                continue;
            }
            let _ = self.docs_tx.send(plan.repo.clone());
            book.next += 1;
            let view = SessionView {
                id: book.next,
                repo: plan.repo.clone(),
                kind: SessionKind::Master,
                doc: Some(plan.number),
                runner: placement[0].clone(),
                machines: placement[1..]
                    .iter()
                    .map(|name| self.machine(name, &views))
                    .collect(),
                state: SessionState::Starting,
                terminal: None,
                started: now,
                ended: None,
                code: None,
                message: String::new(),
            };
            eprintln!(
                "[ci] session {} master {}#{} on {}",
                view.id,
                plan.repo,
                plan.number,
                placement.join(", ")
            );
            let sent = self.spec(&view).is_some_and(|spec| {
                self.runners.send(
                    &view.runner,
                    ToRunner::OpenSession {
                        spec: Box::new(spec),
                    },
                )
            });
            let id = view.id;
            book.list.push(view);
            if !sent {
                self.fail(&mut book, id, "the runner went away", now);
            }
            self.changed(&mut book);
        }
    }

    /// Marks session `id` failed with `message`; a master's plan goes back
    /// to the queue and its host gets no other master for a while.
    fn fail(&self, book: &mut Book, id: u64, message: &str, now: u64) {
        let Some(session) = book.list.iter_mut().find(|s| s.id == id) else {
            return;
        };
        session.state = SessionState::Failed;
        session.message = message.to_owned();
        session.ended = Some(now);
        if session.kind == SessionKind::Master {
            lock(&self.cooldown).insert(session.runner.clone(), now + COOLDOWN_SECS);
            if let Some(number) = session.doc {
                let requeued = {
                    let _held = lock(&self.docs_lock);
                    docs::set_state(
                        &self.state_dir,
                        &self.config,
                        &session.repo,
                        number,
                        DocState::Approved,
                        now,
                    )
                };
                match requeued {
                    Ok(()) => {
                        let _ = self.docs_tx.send(session.repo.clone());
                    }
                    Err(e) => eprintln!("[ci] sessions: {}#{number}: {e}", session.repo),
                }
            }
        }
    }

    /// What runner `name` reports about one of its sessions.
    pub fn on_runner(&self, name: &str, message: FromRunner, now: u64) {
        let mut book = lock(&self.book);
        let id = match &message {
            FromRunner::SessionOpened { id, .. }
            | FromRunner::SessionFailed { id, .. }
            | FromRunner::SessionEnded { id, .. } => *id,
            _ => return,
        };
        if !book.list.iter().any(|s| s.id == id && s.runner == name) {
            eprintln!("[ci] {name}: dropped a report of session {id}, which is not its");
            return;
        }
        match message {
            FromRunner::SessionOpened { terminal, .. } => {
                if let Some(session) = book.list.iter_mut().find(|s| s.id == id) {
                    session.state = SessionState::Running;
                    session.terminal = Some(terminal);
                }
            }
            FromRunner::SessionFailed { message, .. } => {
                eprintln!("[ci] {name}: session {id} failed: {message}");
                self.fail(&mut book, id, &message, now);
            }
            FromRunner::SessionEnded { code, killed, .. } => {
                if let Some(session) = book.list.iter_mut().find(|s| s.id == id) {
                    end(session, code, killed, now);
                }
            }
            _ => {}
        }
        self.changed(&mut book);
    }

    /// Runner `name` enlisted again and holds `reports`: a live session it
    /// does not report ended with it, and a reported end applies.
    pub fn resync(&self, name: &str, reports: &[SessionReport], now: u64) {
        let mut book = lock(&self.book);
        let mut touched = false;
        for session in book
            .list
            .iter_mut()
            .filter(|s| s.runner == name && s.state.is_live())
        {
            touched = true;
            match reports.iter().find(|r| r.id == session.id) {
                None => {
                    end(session, None, true, now);
                    session.message = "the runner no longer holds it".to_owned();
                }
                Some(SessionReport {
                    end: Some((code, killed)),
                    ..
                }) => end(session, *code, *killed, now),
                Some(report) => {
                    session.state = SessionState::Running;
                    session.terminal = Some(report.terminal);
                }
            }
        }
        if touched {
            self.changed(&mut book);
        }
    }

    /// Opens a plan session of `repo` (planning `proposal` when named) on a
    /// runner that hosts sessions: `runner` when named, else the first that
    /// matches the proposal's first `Runs on`, else the first. A live plan
    /// session for the same repository and proposal is answered as it is.
    pub fn open_plan(
        &self,
        repo: &str,
        proposal: Option<u64>,
        runner: Option<&str>,
        now: u64,
    ) -> Result<SessionView, String> {
        if !self.config.repos.contains_key(repo) {
            return Err(format!("no repository {repo}"));
        }
        let mut wanted = Selector::default();
        if let Some(number) = proposal {
            let doc = docs::load(&self.state_dir, &self.config, repo, number)
                .ok_or_else(|| format!("no proposal #{number} in {repo}"))?;
            if doc.kind != DocKind::Proposal || doc.state != DocState::Open {
                return Err(format!("#{number} of {repo} is no open proposal"));
            }
            if let Some(first) = parse_runs_on(&doc.runs_on)?.into_iter().next() {
                wanted = first;
            }
        }
        let mut book = lock(&self.book);
        if let Some(live) = book.list.iter().find(|s| {
            s.kind == SessionKind::Plan && s.repo == repo && s.doc == proposal && s.state.is_live()
        }) {
            return Ok(live.clone());
        }
        let views = self.runners.views();
        let hosts = self.session_hosts(&views);
        let host = match runner {
            Some(named) => hosts
                .iter()
                .find(|(name, _)| name == named)
                .map(|(name, _)| name.clone())
                .ok_or_else(|| format!("runner {named} cannot host a session now"))?,
            None => hosts
                .iter()
                .find(|(_, labels)| wanted.matches(labels))
                .or_else(|| hosts.first())
                .map(|(name, _)| name.clone())
                .ok_or_else(|| {
                    "no runner can host a session (sessions = true and connected)".to_owned()
                })?,
        };
        book.next += 1;
        let view = SessionView {
            id: book.next,
            repo: repo.to_owned(),
            kind: SessionKind::Plan,
            doc: proposal,
            runner: host,
            machines: Vec::new(),
            state: SessionState::Starting,
            terminal: None,
            started: now,
            ended: None,
            code: None,
            message: String::new(),
        };
        let sent = self.spec(&view).is_some_and(|spec| {
            self.runners.send(
                &view.runner,
                ToRunner::OpenSession {
                    spec: Box::new(spec),
                },
            )
        });
        let id = view.id;
        book.list.push(view.clone());
        if !sent {
            self.fail(&mut book, id, "the runner went away", now);
            self.changed(&mut book);
            return Err(format!("runner {} went away", view.runner));
        }
        self.changed(&mut book);
        Ok(view)
    }

    /// Asks the runner of live session `id` to end it.
    pub fn close(&self, id: u64) -> Result<(), String> {
        let book = lock(&self.book);
        let session = book
            .list
            .iter()
            .find(|s| s.id == id && s.state.is_live())
            .ok_or_else(|| format!("session {id} is not running"))?;
        if self
            .runners
            .send(&session.runner, ToRunner::CloseSession { id })
        {
            Ok(())
        } else {
            Err(format!("session {id} is not running"))
        }
    }
}

/// Session ended with `code`, or `killed` without one.
fn end(session: &mut SessionView, code: Option<i32>, killed: bool, now: u64) {
    session.state = SessionState::Ended;
    session.code = code;
    session.ended = Some(now);
    session.message = match (code, killed) {
        (_, true) => "killed".to_owned(),
        (Some(code), false) => format!("exit {code}"),
        (None, false) => "ended without a code".to_owned(),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labelled(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        list.iter()
            .map(|(name, labels)| {
                (
                    (*name).to_owned(),
                    labels.iter().map(|l| (*l).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn selectors(list: &[&str]) -> Vec<Selector> {
        parse_runs_on(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_plan_is_placed_by_its_selectors() {
        let hosts = labelled(&[
            ("atik", &["os=macos", "runner=atik"]),
            ("tuco-pc-hannes", &["os=linux", "runner=tuco-pc-hannes"]),
        ]);
        let extras = labelled(&[
            ("atik", &["os=macos"]),
            ("wohnzimmer", &["os=windows", "arch=amd64"]),
        ]);
        // Any runner: the first in name order.
        assert_eq!(
            place(&selectors(&[]), &hosts, &extras),
            Some(vec!["atik".to_owned()])
        );
        assert_eq!(
            place(&selectors(&["os=linux"]), &hosts, &extras),
            Some(vec!["tuco-pc-hannes".to_owned()])
        );
        // A second machine comes from the ssh runners, never the host itself.
        assert_eq!(
            place(&selectors(&["os=linux", "os=windows"]), &hosts, &extras),
            Some(vec!["tuco-pc-hannes".to_owned(), "wohnzimmer".to_owned()])
        );
        assert_eq!(
            place(&selectors(&["os=macos", "os=macos"]), &hosts, &extras),
            None
        );
        assert_eq!(place(&selectors(&["os=windows"]), &hosts, &extras), None);
        // A busy runner is not in the lists at all.
        assert_eq!(place(&selectors(&["os=linux"]), &hosts[..1], &extras), None);
    }

    /// Tests that touch the filesystem: the integration tier.
    mod io {
        use super::*;
        use std::sync::mpsc::{Receiver, channel};
        use tokio::sync::mpsc::UnboundedReceiver;
        use zeughaus_link::enlist::Offer;

        fn scratch(name: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("zci-sessions-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        const FP: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";

        fn config() -> Arc<CiConfig> {
            Arc::new(
                CiConfig::parse(&format!(
                    "default_runner = \"a\"\n\
                     [runners.a]\nfingerprint = \"{FP}\"\nsessions = true\n\
                     [runners.w]\nfingerprint = \"{FP}\"\nos = \"windows\"\nssh = \"z@w\"\n\
                     [repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\n"
                ))
                .unwrap(),
            )
        }

        fn connect(runners: &Runners, name: &str, os: &str) -> UnboundedReceiver<ToRunner> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            runners.insert(
                name,
                crate::runners::Connected {
                    tx,
                    offer: Offer {
                        os: os.to_owned(),
                        arch: "x86_64".to_owned(),
                        cpus: 1,
                        containers: false,
                        machines: Vec::new(),
                    },
                    build: 0,
                    machines: BTreeMap::new(),
                    since: 0,
                    addresses: Vec::new(),
                },
            );
            rx
        }

        struct Fixture {
            state: PathBuf,
            config: Arc<CiConfig>,
            runners: Arc<Runners>,
            sessions: Sessions,
            _docs: Receiver<String>,
            _events: Receiver<()>,
        }

        fn fixture(name: &str) -> Fixture {
            let state = scratch(name);
            let config = config();
            let runners = Arc::new(Runners::new());
            let (docs_tx, docs) = channel();
            let (events_tx, events) = channel();
            let sessions = Sessions::new(
                state.clone(),
                Arc::clone(&config),
                Arc::clone(&runners),
                Arc::new(Mutex::new(())),
                docs_tx,
                events_tx,
            );
            Fixture {
                state,
                config,
                runners,
                sessions,
                _docs: docs,
                _events: events,
            }
        }

        fn approved_plan(f: &Fixture, runs_on: &[&str]) -> u64 {
            let runs_on: Vec<String> = runs_on.iter().map(|s| (*s).to_owned()).collect();
            let number = docs::create(
                &f.state,
                &f.config,
                "demo",
                docs::NewDoc {
                    kind: DocKind::Plan,
                    title: "p",
                    body: "body",
                    source: "demo",
                    proposals: &[],
                    runs_on: &runs_on,
                },
                1,
            )
            .unwrap();
            docs::set_state(&f.state, &f.config, "demo", number, DocState::Approved, 2).unwrap();
            number
        }

        fn state_of(f: &Fixture, number: u64) -> DocState {
            docs::load(&f.state, &f.config, "demo", number)
                .unwrap()
                .state
        }

        #[test]
        fn one_master_per_runner_and_a_failed_start_requeues_the_plan() {
            let f = fixture("dispatch");
            let mut a = connect(&f.runners, "a", "linux");
            let mut w = connect(&f.runners, "w", "windows");
            let first = approved_plan(&f, &["os=linux", "os=windows"]);
            let second = approved_plan(&f, &[]);
            let third = approved_plan(&f, &["os=macos"]);

            f.sessions.dispatch(10);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the host was sent the session");
            };
            assert_eq!((spec.kind, spec.doc), (SessionKind::Master, Some(first)));
            assert_eq!(spec.machines[0].ssh.as_deref(), Some("z@w"));
            assert!(w.try_recv().is_err(), "a machine is not sent the session");
            assert_eq!(state_of(&f, first), DocState::InProgress);
            // `a` is busy and nothing else hosts: both stay queued.
            assert_eq!(state_of(&f, second), DocState::Approved);
            assert_eq!(state_of(&f, third), DocState::Approved);

            f.sessions.on_runner(
                "a",
                FromRunner::SessionOpened {
                    id: spec.id,
                    terminal: 4,
                },
                11,
            );
            assert_eq!(
                f.sessions.session(spec.id).unwrap().state,
                SessionState::Running
            );
            f.sessions.on_runner(
                "a",
                FromRunner::SessionEnded {
                    id: spec.id,
                    code: Some(0),
                    killed: false,
                },
                12,
            );
            // The plan stays in progress for the user or plan_done to settle.
            assert_eq!(state_of(&f, first), DocState::InProgress);

            // `a` is free again: the next plan goes there, and fails.
            f.sessions.dispatch(13);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the second plan was sent");
            };
            assert_eq!(spec.doc, Some(second));
            f.sessions.on_runner(
                "a",
                FromRunner::SessionFailed {
                    id: spec.id,
                    message: "no omp".to_owned(),
                },
                14,
            );
            let failed = f.sessions.session(spec.id).unwrap();
            assert_eq!(
                (failed.state, failed.message.as_str()),
                (SessionState::Failed, "no omp")
            );
            assert_eq!(state_of(&f, second), DocState::Approved);
            // The host cools down.
            f.sessions.dispatch(15);
            assert!(a.try_recv().is_err());
            f.sessions.dispatch(15 + COOLDOWN_SECS);
            assert!(matches!(a.try_recv(), Ok(ToRunner::OpenSession { .. })));
        }

        #[test]
        fn a_reconnecting_runner_ends_the_sessions_it_no_longer_holds() {
            let f = fixture("resync");
            let _a = connect(&f.runners, "a", "linux");
            // `w` hosts no sessions.
            assert_eq!(
                f.sessions.open_plan("demo", None, Some("w"), 1),
                Err("runner w cannot host a session now".to_owned())
            );
            let kept = f.sessions.open_plan("demo", None, None, 1).unwrap();
            assert_eq!(kept.runner, "a");
            // Asked again: the live session is the answer.
            assert_eq!(
                f.sessions.open_plan("demo", None, None, 2).unwrap().id,
                kept.id
            );
            approved_plan(&f, &[]);
            f.sessions.dispatch(3);
            let master = f.sessions.listed()[1].id;
            f.sessions.resync(
                "a",
                &[SessionReport {
                    id: kept.id,
                    terminal: 7,
                    end: None,
                }],
                4,
            );
            assert_eq!(f.sessions.session(kept.id).unwrap().terminal, Some(7));
            let lost = f.sessions.session(master).unwrap();
            assert_eq!(lost.state, SessionState::Ended);
            assert!(f.sessions.close(master).is_err());
            assert!(f.sessions.close(kept.id).is_ok());
        }
    }
}
