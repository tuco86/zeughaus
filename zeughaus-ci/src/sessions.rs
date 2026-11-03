//! Agent sessions: the registry of the sessions the CI started on its
//! runners, and the dispatcher that starts a plan session for each ready
//! ticket and a master session for each approved one, each once the
//! tickets it depends on are finished.
//!
//! A master session executes one ticket's plan and hosts nothing else: a
//! runner runs at most one master at a time, and the runners a master
//! reaches over ssh are reserved for it too, so several plans run at once
//! on different runners. Plan sessions (omp in plan mode) never count as
//! busy and reserve nothing; a ticket's plan session after a Refine resumes
//! on the runner that wrote its plan.
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
    ClaimHolder, ClaimSubject, ClaimView, DocKind, DocState, DocView, SessionKind, SessionMachine,
    SessionState, SessionView, normalize_origin,
};

use crate::claims;
use crate::config::{CiConfig, Shared};
use crate::docs;
use crate::files::{read_json, write_json};
use crate::runners::{RunnerView, Runners};
use crate::scheduler::{JobStatus, PipelineStatus};

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
    /// The live claims; renewals write them without an announcement.
    #[serde(default)]
    claims: Vec<ClaimView>,
}

pub struct Sessions {
    state_dir: PathBuf,
    config: Arc<Shared>,
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
    /// Alerts the main loop publishes to editors: a master that could not
    /// start or ended by itself.
    alerts_tx: Sender<crate::scheduler::Alert>,
    /// Live sessions the user stopped with `close`; their end restarts
    /// nothing.
    stopping: Mutex<BTreeSet<u64>>,
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
        config: Arc<Shared>,
        runners: Arc<Runners>,
        docs_lock: Arc<Mutex<()>>,
        docs_tx: Sender<String>,
        events_tx: Sender<()>,
        alerts_tx: Sender<crate::scheduler::Alert>,
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
            alerts_tx,
            stopping: Mutex::new(BTreeSet::new()),
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
        self.write(book);
        let _ = self.events_tx.send(());
    }

    /// Writes the book without announcing it: for a renewal, which changes
    /// nothing an editor shows.
    fn write(&self, book: &Book) {
        if let Err(e) = write_json(&book_path(&self.state_dir), book) {
            eprintln!("[ci] sessions: {e}");
        }
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
        let config = self.config.get();
        config
            .runners
            .iter()
            .filter(|(_, runner)| runner.sessions)
            .filter_map(|(name, _)| {
                let view = views.get(name)?;
                Some((name.clone(), config.labels_of(name, Some(&view.offer))))
            })
            .collect()
    }

    /// What a session needs to know of runner `name` as one of its machines.
    fn machine(&self, name: &str, views: &BTreeMap<String, RunnerView>) -> SessionMachine {
        let view = views.get(name);
        let config = self.config.get();
        SessionMachine {
            name: name.to_owned(),
            ssh: config.runners.get(name).and_then(|r| r.ssh.clone()),
            labels: config.labels_of(name, view.map(|v| &v.offer)),
            addresses: view.map(|v| v.addresses.clone()).unwrap_or_default(),
        }
    }

    /// What the runner of `view` is sent to open it; `resume` is the omp
    /// session file a plan session resumes.
    fn spec(&self, view: &SessionView, resume: Option<String>) -> Option<SessionSpec> {
        let config = self.config.get();
        let repo = config.repos.get(&view.repo)?;
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
            resume,
        })
    }

    /// Whether ticket `doc` waits for a `depends on` ticket; the docs
    /// functions that list tickets for dispatch fill `waits_for`.
    fn blocked(doc: &DocView) -> bool {
        !doc.waits_for.is_empty()
    }

    /// Starts a plan session for each ready ticket and a master for each
    /// approved ticket that finds free runners, in queue order, skipping
    /// those that wait for their dependencies. A ticket that finds no
    /// runner stays queued; a later one may still find its own.
    pub fn dispatch(&self, now: u64) {
        {
            let mut book = lock(&self.book);
            if claims::expire(&mut book.claims, now) {
                self.changed(&mut book);
            }
        }
        let config = self.config.get();
        let views = self.runners.views();
        self.dispatch_planning(&config, &views, now);
        let queue = docs::queue(&self.state_dir, &config);
        if queue.is_empty() {
            return;
        }
        for plan in queue {
            if docs::paused(&self.state_dir, &config, &plan.repo) {
                continue;
            }
            if Self::blocked(&plan) {
                continue;
            }
            let Ok(selectors) = parse_runs_on(&plan.runs_on) else {
                continue;
            };
            let mut book = lock(&self.book);
            let busy = Self::busy(&book);
            let cooldown = lock(&self.cooldown).clone();
            let free = |name: &String| !busy.contains(name);
            // The runner the ticket's master last ran on holds its clone
            // and omp session: it may take the ticket back while cooling
            // down, and it comes first.
            let previous = plan.placement.first();
            let mut hosts: Vec<(String, Vec<String>)> = self
                .session_hosts(&views)
                .into_iter()
                .filter(|(name, _)| {
                    free(name)
                        && (Some(name) == previous || cooldown.get(name).is_none_or(|&t| t <= now))
                })
                .collect();
            hosts.sort_by_key(|(name, _)| Some(name) != previous);
            let extras: Vec<(String, Vec<String>)> = config
                .runners
                .iter()
                .filter(|(name, runner)| runner.ssh.is_some() && free(name))
                .filter_map(|(name, _)| {
                    let view = views.get(name)?;
                    Some((name.clone(), config.labels_of(name, Some(&view.offer))))
                })
                .collect();
            let Some(placement) = place(&selectors, &hosts, &extras) else {
                continue;
            };
            let started = {
                let _held = lock(&self.docs_lock);
                docs::start(
                    &self.state_dir,
                    &config,
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
            let sent = self.spec(&view, None).is_some_and(|spec| {
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

    /// Opens a plan session for each ready ticket whose dependencies are
    /// finished and that nobody plans yet: on the runner that wrote its
    /// plan when it has one (it waits while that runner is away), else on
    /// the first session host its first `Runs on` matches, else the first.
    fn dispatch_planning(&self, config: &CiConfig, views: &BTreeMap<String, RunnerView>, now: u64) {
        for ticket in docs::ready(&self.state_dir, config) {
            if Self::blocked(&ticket) {
                continue;
            }
            let mut book = lock(&self.book);
            let subject = ClaimSubject::Ticket {
                repo: ticket.repo.clone(),
                number: ticket.number,
            };
            if book.claims.iter().any(|claim| claim.subject == subject) {
                continue;
            }
            if book.list.iter().any(|s| {
                s.kind == SessionKind::Plan
                    && s.repo == ticket.repo
                    && s.doc == Some(ticket.number)
                    && s.state.is_live()
            }) {
                continue;
            }
            let hosts = self.session_hosts(views);
            let host = match &ticket.planner {
                Some(planner) => hosts
                    .iter()
                    .find(|(name, _)| *name == planner.runner)
                    .map(|(name, _)| name.clone()),
                None => {
                    let wanted = parse_runs_on(&ticket.runs_on)
                        .ok()
                        .and_then(|selectors| selectors.into_iter().next())
                        .unwrap_or_default();
                    hosts
                        .iter()
                        .find(|(_, labels)| wanted.matches(labels))
                        .or_else(|| hosts.first())
                        .map(|(name, _)| name.clone())
                }
            };
            let Some(host) = host else {
                continue;
            };
            let started = {
                let _held = lock(&self.docs_lock);
                docs::start_planning(&self.state_dir, config, &ticket.repo, ticket.number, now)
            };
            let doc = match started {
                Ok(doc) => doc,
                Err(e) => {
                    eprintln!("[ci] sessions: {}#{}: {e}", ticket.repo, ticket.number);
                    continue;
                }
            };
            let _ = self.docs_tx.send(ticket.repo.clone());
            book.next += 1;
            let view = SessionView {
                id: book.next,
                repo: ticket.repo.clone(),
                kind: SessionKind::Plan,
                doc: Some(ticket.number),
                runner: host,
                machines: Vec::new(),
                state: SessionState::Starting,
                terminal: None,
                started: now,
                ended: None,
                code: None,
                message: String::new(),
            };
            let resume = doc.planner.map(|planner| planner.omp_session);
            eprintln!(
                "[ci] session {} plan {}#{} on {}{}",
                view.id,
                ticket.repo,
                ticket.number,
                view.runner,
                if resume.is_some() { " (resuming)" } else { "" }
            );
            let sent = self.spec(&view, resume).is_some_and(|spec| {
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

    /// Plan session `session` ended: its ticket, when still in planning, got
    /// no plan and goes back with `note`. The caller holds the book.
    fn plan_ended(&self, session: &SessionView, note: &str, now: u64) {
        let (SessionKind::Plan, Some(number)) = (session.kind, session.doc) else {
            return;
        };
        let ended = {
            let _held = lock(&self.docs_lock);
            docs::planning_ended(
                &self.state_dir,
                &self.config.get(),
                &session.repo,
                number,
                note,
                now,
            )
        };
        match ended {
            Ok(true) => {
                let _ = self.docs_tx.send(session.repo.clone());
            }
            Ok(false) => {}
            Err(e) => eprintln!("[ci] sessions: {}#{number}: {e}", session.repo),
        }
    }

    /// The master of `session` ended: `lost` when it ended without an exit
    /// code (the runner or a signal killed it) or went away with its runner,
    /// which requeues its ticket for that runner to resume; a master stopped
    /// with `close` or exiting by itself leaves the ticket in progress with
    /// a note. The caller holds the book.
    fn master_gone(&self, session: &SessionView, lost: bool, now: u64) {
        let stopped = lock(&self.stopping).remove(&session.id);
        let (SessionKind::Master, Some(number)) = (session.kind, session.doc) else {
            return;
        };
        let end = if stopped {
            docs::MasterEnd::Stopped("it was stopped")
        } else if lost {
            docs::MasterEnd::Lost
        } else {
            docs::MasterEnd::Stopped(&session.message)
        };
        let alert = matches!(end, docs::MasterEnd::Stopped(_)) && !stopped;
        let config = self.config.get();
        let ended = {
            let _held = lock(&self.docs_lock);
            docs::master_ended(&self.state_dir, &config, &session.repo, number, end, now)
        };
        match ended {
            Ok(true) => {
                let _ = self.docs_tx.send(session.repo.clone());
                if alert {
                    let note = docs::load(&self.state_dir, &config, &session.repo, number)
                        .map(|doc| doc.note)
                        .unwrap_or_default();
                    let _ = self.alerts_tx.send(crate::scheduler::Alert {
                        title: format!("{} #{number}: the master ended", session.repo),
                        body: note,
                    });
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!("[ci] sessions: {}#{number}: {e}", session.repo),
        }
    }

    /// Marks session `id` failed with `message`; a master's ticket goes back
    /// to the queue and its host gets no other master for a while, a plan
    /// session's ticket back to where it was.
    fn fail(&self, book: &mut Book, id: u64, message: &str, now: u64) {
        let Some(session) = book.list.iter_mut().find(|s| s.id == id) else {
            return;
        };
        session.state = SessionState::Failed;
        session.message = message.to_owned();
        session.ended = Some(now);
        claims::release_session(&mut book.claims, id, &session.runner, session.terminal);
        if session.kind == SessionKind::Plan {
            let session = session.clone();
            self.plan_ended(
                &session,
                &format!("the plan session failed: {message}"),
                now,
            );
        }
        if session.kind == SessionKind::Master {
            lock(&self.cooldown).insert(session.runner.clone(), now + COOLDOWN_SECS);
            if let Some(number) = session.doc {
                let requeued = {
                    let _held = lock(&self.docs_lock);
                    docs::master_ended(
                        &self.state_dir,
                        &self.config.get(),
                        &session.repo,
                        number,
                        docs::MasterEnd::Refused(message),
                        now,
                    )
                };
                match requeued {
                    Ok(true) => {
                        let _ = self.docs_tx.send(session.repo.clone());
                        let _ = self.alerts_tx.send(crate::scheduler::Alert {
                            title: format!(
                                "{} #{number}: the master could not start",
                                session.repo
                            ),
                            body: message.to_owned(),
                        });
                    }
                    Ok(false) => {}
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
                let book = &mut *book;
                if let Some(session) = book.list.iter_mut().find(|s| s.id == id) {
                    end(session, code, killed, now);
                    claims::release_session(
                        &mut book.claims,
                        id,
                        &session.runner,
                        session.terminal,
                    );
                    let session = session.clone();
                    self.plan_ended(&session, "the plan session ended without a plan", now);
                    // No exit code: the runner or a signal killed it.
                    self.master_gone(&session, killed || code.is_none(), now);
                }
            }
            _ => {}
        }
        self.changed(&mut book);
    }

    /// Runner `name` enlisted again and holds `reports`: a live session it
    /// does not report ended with it, and a reported end applies.
    pub fn resync(&self, name: &str, reports: &[SessionReport], now: u64) {
        let mut guard = lock(&self.book);
        let book = &mut *guard;
        let mut touched = false;
        let mut ended = Vec::new();
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
                    ended.push((session.clone(), true));
                }
                Some(SessionReport {
                    end: Some((code, killed)),
                    ..
                }) => {
                    end(session, *code, *killed, now);
                    ended.push((session.clone(), *killed || code.is_none()));
                }
                Some(report) => {
                    session.state = SessionState::Running;
                    session.terminal = Some(report.terminal);
                }
            }
        }
        for (session, lost) in ended {
            claims::release_session(&mut book.claims, session.id, name, session.terminal);
            self.plan_ended(&session, "the plan session ended with its runner", now);
            self.master_gone(&session, lost, now);
        }
        if touched {
            self.changed(book);
        }
    }

    /// Opens a plan session of `repo` without a ticket on a runner that
    /// hosts sessions: `runner` when named, else the first. Its plan becomes
    /// a new ticket. A live such session of the repository is answered as
    /// it is.
    pub fn open_plan(
        &self,
        repo: &str,
        runner: Option<&str>,
        now: u64,
    ) -> Result<SessionView, String> {
        let config = self.config.get();
        if !config.repos.contains_key(repo) {
            return Err(format!("no repository {repo}"));
        }
        let mut book = lock(&self.book);
        if let Some(live) = book.list.iter().find(|s| {
            s.kind == SessionKind::Plan && s.repo == repo && s.doc.is_none() && s.state.is_live()
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
            None => hosts.first().map(|(name, _)| name.clone()).ok_or_else(|| {
                "no runner can host a session (sessions = true and connected)".to_owned()
            })?,
        };
        book.next += 1;
        let view = SessionView {
            id: book.next,
            repo: repo.to_owned(),
            kind: SessionKind::Plan,
            doc: None,
            runner: host,
            machines: Vec::new(),
            state: SessionState::Starting,
            terminal: None,
            started: now,
            ended: None,
            code: None,
            message: String::new(),
        };
        let sent = self.spec(&view, None).is_some_and(|spec| {
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
            lock(&self.stopping).insert(id);
            Ok(())
        } else {
            Err(format!("session {id} is not running"))
        }
    }

    /// Every live claim.
    pub fn claims(&self) -> Vec<ClaimView> {
        lock(&self.book).claims.clone()
    }

    /// Takes `subject` for `holder` (see [`claims::take`]): a ticket that is
    /// not finished, or a pipeline that exists. The holder's live CI
    /// session, by id or by its terminal, names the claim.
    pub fn claim(
        &self,
        subject: ClaimSubject,
        mut holder: ClaimHolder,
        now: u64,
    ) -> Result<(ClaimView, bool), String> {
        if !crate::config::valid_name(&holder.runner) {
            return Err(format!("runner name {:?} is not valid", holder.runner));
        }
        let config = self.config.get();
        let repo = subject.repo();
        if !config.repos.contains_key(repo) {
            return Err(format!("no repository {repo}"));
        }
        let channel = match &subject {
            ClaimSubject::Ticket { number, .. } => {
                let open = docs::load(&self.state_dir, &config, repo, *number)
                    .is_some_and(|doc| ticket_open(&doc));
                if !open {
                    return Err(format!("#{number} of {repo} is no open ticket"));
                }
                String::new()
            }
            ClaimSubject::Pipeline { number, .. } => {
                crate::scheduler::load_pipeline(&self.state_dir, repo, *number)
                    .map_err(|_| format!("no pipeline {repo} #{number}"))?
                    .channel()
            }
        };
        let mut book = lock(&self.book);
        let session = book.list.iter().find(|s| {
            s.state.is_live()
                && s.runner == holder.runner
                && (holder.session == Some(s.id) || s.terminal == Some(holder.terminal))
        });
        holder.session = session.map(|s| s.id);
        let label = session.map(session_label).unwrap_or_default();
        let before = book.claims.len();
        let (claim, ours) = claims::take(&mut book.claims, subject, holder, label, channel, now);
        if book.claims.len() != before {
            self.changed(&mut book);
        } else if ours {
            self.write(&book);
        }
        Ok((claim, ours))
    }

    /// Renews every claim of `holder`.
    pub fn renew_claims(&self, holder: &ClaimHolder, now: u64) {
        let mut book = lock(&self.book);
        if claims::renew(&mut book.claims, holder, now) > 0 {
            self.write(&book);
        }
    }

    /// Releases `subject` of `holder`, or all its claims without one.
    pub fn release_claims(&self, holder: &ClaimHolder, subject: Option<&ClaimSubject>) {
        let mut book = lock(&self.book);
        if claims::release(&mut book.claims, holder, subject) {
            self.changed(&mut book);
        }
    }

    /// Who is responsible for pipeline `number` of `repo` (see
    /// [`zeughaus_link::responsible`]).
    pub fn responsible(&self, repo: &str, number: u64) -> Result<Option<ClaimView>, String> {
        let record = crate::scheduler::load_pipeline(&self.state_dir, repo, number)
            .map_err(|_| format!("no pipeline {repo} #{number}"))?;
        let claims = self.claims();
        let failed_of = |m| {
            crate::scheduler::load_pipeline(&self.state_dir, repo, m)
                .map(|record| failed_jobs(&record))
                .unwrap_or_default()
        };
        Ok(zeughaus_link::responsible(
            &claims,
            repo,
            number,
            &record.channel(),
            &failed_jobs(&record),
            failed_of,
        )
        .cloned())
    }

    /// Pipeline `number` of `repo` in `channel` changed: a success settles
    /// the channel's claims up to it, a cancel its own.
    pub fn pipeline_changed(&self, repo: &str, channel: &str, number: u64) {
        let held = lock(&self.book).claims.iter().any(|claim| {
            matches!(claim.subject, ClaimSubject::Pipeline { .. })
                && claim.subject.repo() == repo
                && claim.channel == channel
        });
        if !held {
            return;
        }
        let Ok(record) = crate::scheduler::load_pipeline(&self.state_dir, repo, number) else {
            return;
        };
        let succeeded = match record.status {
            PipelineStatus::Succeeded => true,
            PipelineStatus::Cancelled => false,
            _ => return,
        };
        let mut book = lock(&self.book);
        if claims::pipeline_ended(&mut book.claims, repo, channel, number, succeeded) {
            self.changed(&mut book);
        }
    }

    /// `repo`'s documents changed: a claim on a ticket that is finished
    /// goes.
    pub fn docs_changed(&self, repo: &str) {
        let mut book = lock(&self.book);
        let held = book.claims.iter().any(|claim| {
            matches!(claim.subject, ClaimSubject::Ticket { .. }) && claim.subject.repo() == repo
        });
        if !held {
            return;
        }
        let docs = docs::list(&self.state_dir, &self.config.get(), repo);
        let before = book.claims.len();
        book.claims.retain(|claim| match &claim.subject {
            ClaimSubject::Ticket { repo: r, number } if r == repo => docs
                .iter()
                .any(|doc| doc.number == *number && ticket_open(doc)),
            _ => true,
        });
        if book.claims.len() != before {
            self.changed(&mut book);
        }
    }
}

/// A ticket that is not finished: a claim on it means something.
fn ticket_open(doc: &DocView) -> bool {
    doc.kind == DocKind::Ticket
        && !matches!(
            doc.state,
            DocState::Done | DocState::Dismissed | DocState::Merged
        )
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

/// `master zeughaus #38`, `plan zeughaus`: a session as the editor names it.
fn session_label(session: &SessionView) -> String {
    match session.doc {
        Some(doc) => format!("{} {} #{doc}", session.kind.name(), session.repo),
        None => format!("{} {}", session.kind.name(), session.repo),
    }
}

/// The names of a pipeline record's failed jobs.
pub(crate) fn failed_jobs(record: &crate::scheduler::Pipeline) -> Vec<String> {
    record
        .jobs
        .iter()
        .filter(|job| job.status == JobStatus::Failed)
        .map(|job| job.def.name.clone())
        .collect()
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

        use zeughaus_link::Planner;

        use crate::config::CiConfig;

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
            alerts: Receiver<crate::scheduler::Alert>,
            _docs: Receiver<String>,
            _events: Receiver<()>,
        }

        fn fixture(name: &str) -> Fixture {
            let state = scratch(name);
            let config = config();
            let runners = Arc::new(Runners::new());
            let (docs_tx, docs) = channel();
            let (events_tx, events) = channel();
            let (alerts_tx, alerts) = channel();
            let sessions = Sessions::new(
                state.clone(),
                Shared::new(CiConfig::clone(&config)),
                Arc::clone(&runners),
                Arc::new(Mutex::new(())),
                docs_tx,
                events_tx,
                alerts_tx,
            );
            Fixture {
                state,
                config,
                runners,
                sessions,
                alerts,
                _docs: docs,
                _events: events,
            }
        }

        fn approved_plan(f: &Fixture, runs_on: &[&str]) -> u64 {
            let runs_on: Vec<String> = runs_on.iter().map(|s| (*s).to_owned()).collect();
            let number = docs::write_plan(
                &f.state,
                &f.config,
                "demo",
                None,
                docs::PlanText {
                    title: "p",
                    plan: "# p\n",
                    includes: &[],
                    runs_on: &runs_on,
                    source: "demo",
                    planner: None,
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
                f.sessions.open_plan("demo", Some("w"), 1),
                Err("runner w cannot host a session now".to_owned())
            );
            let kept = f.sessions.open_plan("demo", None, 1).unwrap();
            assert_eq!(kept.runner, "a");
            // Asked again: the live session is the answer.
            assert_eq!(f.sessions.open_plan("demo", None, 2).unwrap().id, kept.id);
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

        #[test]
        fn a_killed_master_is_started_again_on_its_runner() {
            let f = fixture("restart");
            let mut a = connect(&f.runners, "a", "linux");
            let number = approved_plan(&f, &[]);
            f.sessions.dispatch(10);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the master was sent");
            };
            let report = |message: FromRunner, at: u64| f.sessions.on_runner("a", message, at);
            report(
                FromRunner::SessionOpened {
                    id: spec.id,
                    terminal: 4,
                },
                11,
            );
            // `kill -9` from outside: the runner did not kill it, and it
            // ended without a code.
            report(
                FromRunner::SessionEnded {
                    id: spec.id,
                    code: None,
                    killed: false,
                },
                20,
            );
            assert_eq!(state_of(&f, number), DocState::Approved);

            // Its runner takes it back even while cooling down.
            lock(&f.sessions.cooldown).insert("a".into(), 1000);
            f.sessions.dispatch(21);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the master was started again on its runner");
            };
            assert_eq!(spec.doc, Some(number));
            let doc = docs::load(&f.state, &f.config, "demo", number).unwrap();
            assert_eq!((doc.state, doc.note.as_str()), (DocState::InProgress, ""));

            // Stopped by the user: it stays in progress and is not restarted.
            report(
                FromRunner::SessionOpened {
                    id: spec.id,
                    terminal: 5,
                },
                22,
            );
            f.sessions.close(spec.id).unwrap();
            assert!(matches!(
                a.try_recv(),
                Ok(ToRunner::CloseSession { id }) if id == spec.id
            ));
            report(
                FromRunner::SessionEnded {
                    id: spec.id,
                    code: None,
                    killed: true,
                },
                23,
            );
            let doc = docs::load(&f.state, &f.config, "demo", number).unwrap();
            assert_eq!(doc.state, DocState::InProgress);
            assert!(doc.note.ends_with("Requeue resumes it"), "{}", doc.note);
            f.sessions.dispatch(30);
            assert!(a.try_recv().is_err());
        }

        #[test]
        fn a_refused_master_requeues_its_ticket_with_the_reason() {
            let f = fixture("refused");
            let mut a = connect(&f.runners, "a", "linux");
            let number = approved_plan(&f, &[]);
            f.sessions.dispatch(10);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the master was sent");
            };
            f.sessions.on_runner(
                "a",
                FromRunner::SessionFailed {
                    id: spec.id,
                    message: "holds 2 changed files of ticket #9".to_owned(),
                },
                11,
            );
            let doc = docs::load(&f.state, &f.config, "demo", number).unwrap();
            assert_eq!(doc.state, DocState::Approved);
            assert!(doc.placement.is_empty());
            assert_eq!(
                doc.note,
                "the master could not start: holds 2 changed files of ticket #9"
            );
            let alert = f.alerts.try_recv().unwrap();
            assert_eq!(
                alert.title,
                format!("demo #{number}: the master could not start")
            );
        }

        fn open_ticket(f: &Fixture, depends: &[u64]) -> u64 {
            let mut body = String::from("# t\n");
            if !depends.is_empty() {
                let refs: Vec<String> = depends.iter().map(|n| format!("#{n}")).collect();
                body.push_str(&format!("Depends on: {}\n", refs.join(", ")));
            }
            body.push_str("\nbody\n");
            docs::create(
                &f.state,
                &f.config,
                "demo",
                docs::NewDoc {
                    kind: DocKind::Ticket,
                    body: &body,
                    source: "elsewhere",
                    runs_on: &[],
                    onboard: None,
                },
                1,
            )
            .unwrap()
        }

        fn ready_ticket(f: &Fixture, depends: &[u64]) -> u64 {
            let number = open_ticket(f, depends);
            docs::set_state(&f.state, &f.config, "demo", number, DocState::Ready, 2).unwrap();
            number
        }

        fn holder(terminal: u64) -> ClaimHolder {
            ClaimHolder {
                runner: "a".to_owned(),
                terminal,
                session: None,
            }
        }

        fn ticket(number: u64) -> ClaimSubject {
            ClaimSubject::Ticket {
                repo: "demo".to_owned(),
                number,
            }
        }

        #[test]
        fn a_ready_ticket_gets_a_plan_session_once_its_dependencies_are_done() {
            let f = fixture("planning");
            let mut a = connect(&f.runners, "a", "linux");
            let first = open_ticket(&f, &[]);
            let waiting = ready_ticket(&f, &[first]);
            let free = ready_ticket(&f, &[]);

            f.sessions.dispatch(10);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the free ticket gets a plan session");
            };
            assert_eq!(
                (spec.kind, spec.doc, spec.resume.clone()),
                (SessionKind::Plan, Some(free), None)
            );
            assert!(a.try_recv().is_err(), "the waiting ticket gets none");
            assert_eq!(state_of(&f, free), DocState::Planning);
            assert_eq!(state_of(&f, waiting), DocState::Ready);

            // The session ends without a plan: the ticket goes back open.
            f.sessions.on_runner(
                "a",
                FromRunner::SessionEnded {
                    id: spec.id,
                    code: Some(0),
                    killed: false,
                },
                11,
            );
            let back = docs::load(&f.state, &f.config, "demo", free).unwrap();
            assert_eq!(
                (back.state, back.note.as_str()),
                (DocState::Open, "the plan session ended without a plan")
            );

            docs::set_state(&f.state, &f.config, "demo", first, DocState::Dismissed, 12).unwrap();
            f.sessions.dispatch(13);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the unblocked ticket gets a plan session");
            };
            assert_eq!(spec.doc, Some(waiting));
        }

        #[test]
        fn a_refined_ticket_resumes_on_the_runner_that_planned_it() {
            let f = fixture("resume");
            let refined = |runner: &str| {
                let number = ready_ticket(&f, &[]);
                docs::start_planning(&f.state, &f.config, "demo", number, 3).unwrap();
                let planner = Planner {
                    runner: runner.to_owned(),
                    omp_session: format!("/s/{runner}.jsonl"),
                };
                let text = docs::PlanText {
                    title: "p",
                    plan: "# p\n",
                    includes: &[],
                    runs_on: &[],
                    source: "demo",
                    planner: Some(planner),
                };
                docs::write_plan(&f.state, &f.config, "demo", Some(number), text, 4).unwrap();
                docs::refine(&f.state, &f.config, "demo", number, "shorter", 5).unwrap();
                number
            };
            // Planned on `w`, which hosts no sessions: the ticket waits.
            let elsewhere = refined("w");
            let mut a = connect(&f.runners, "a", "linux");
            f.sessions.dispatch(6);
            assert!(a.try_recv().is_err());
            assert_eq!(state_of(&f, elsewhere), DocState::Ready);

            let here = refined("a");
            f.sessions.dispatch(7);
            let Ok(ToRunner::OpenSession { spec }) = a.try_recv() else {
                panic!("the planner's runner resumes it");
            };
            assert_eq!(
                (spec.doc, spec.resume.as_deref()),
                (Some(here), Some("/s/a.jsonl"))
            );
        }

        #[test]
        fn the_first_holder_of_a_ticket_keeps_it_from_the_planning_dispatch() {
            let f = fixture("claim-ticket");
            let mut a = connect(&f.runners, "a", "linux");
            let number = open_ticket(&f, &[]);
            let (claim, ours) = f.sessions.claim(ticket(number), holder(3), 5).unwrap();
            assert!(ours);
            assert_eq!(claim.since, 5);
            let (theirs, ours) = f.sessions.claim(ticket(number), holder(4), 6).unwrap();
            assert!(!ours);
            assert_eq!(theirs.holder.terminal, 3);
            docs::set_state(&f.state, &f.config, "demo", number, DocState::Ready, 7).unwrap();
            f.sessions.dispatch(7);
            assert!(a.try_recv().is_err(), "a claimed ticket is planned already");
            assert_eq!(
                f.sessions.claim(ticket(99), holder(3), 8),
                Err("#99 of demo is no open ticket".to_owned())
            );
            let bad = ClaimHolder {
                runner: "A B".to_owned(),
                ..holder(3)
            };
            assert!(f.sessions.claim(ticket(number), bad, 8).is_err());
        }

        #[test]
        fn an_ended_session_drops_the_claims_of_its_terminal() {
            let f = fixture("claim-session");
            let _a = connect(&f.runners, "a", "linux");
            let number = open_ticket(&f, &[]);
            let session = f.sessions.open_plan("demo", None, 1).unwrap();
            f.sessions.on_runner(
                "a",
                FromRunner::SessionOpened {
                    id: session.id,
                    terminal: 9,
                },
                2,
            );
            let (claim, _) = f.sessions.claim(ticket(number), holder(9), 3).unwrap();
            assert_eq!(claim.holder.session, Some(session.id));
            assert_eq!(claim.label, "plan demo");
            f.sessions.on_runner(
                "a",
                FromRunner::SessionEnded {
                    id: session.id,
                    code: Some(0),
                    killed: false,
                },
                4,
            );
            assert!(f.sessions.claims().is_empty());
        }

        fn write_pipeline(f: &Fixture, number: u64, status: &str, failed: &[&str]) {
            let dir = crate::ci_dir(&f.state).join("pipelines").join("demo");
            std::fs::create_dir_all(&dir).unwrap();
            let event = crate::event::CiEvent {
                repo: "demo".to_owned(),
                kind: crate::event::EventKind::Push,
                git_ref: "main".to_owned(),
                sha: None,
                actor: "test".to_owned(),
                delivery: None,
                upload: None,
                upstream: None,
                received: 0,
            };
            let jobs: Vec<serde_json::Value> = failed
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "def": {
                            "name": name, "file": format!("{name}.sh"), "on": [], "needs": [],
                            "image": null, "machine": null, "secrets": [], "env": {},
                            "timeout_minutes": 30, "interpreter": "sh",
                        },
                        "status": "failed",
                    })
                })
                .collect();
            let record = serde_json::json!({
                "repo": "demo", "number": number, "key": "push main", "event": event,
                "sha": null, "status": status, "created": 0, "jobs": jobs, "channel": "dev",
            });
            std::fs::write(dir.join(format!("{number}.json")), record.to_string()).unwrap();
        }

        #[test]
        fn a_later_success_of_the_channel_settles_its_pipeline_claims() {
            let f = fixture("claim-pipeline");
            write_pipeline(&f, 1, "failed", &["check"]);
            write_pipeline(&f, 2, "failed", &["check"]);
            let pipeline = |number| ClaimSubject::Pipeline {
                repo: "demo".to_owned(),
                number,
            };
            let (claim, _) = f.sessions.claim(pipeline(1), holder(3), 1).unwrap();
            assert_eq!(claim.channel, "dev");
            f.sessions.claim(pipeline(2), holder(4), 1).unwrap();
            assert_eq!(
                f.sessions.claim(pipeline(9), holder(4), 1),
                Err("no pipeline demo #9".to_owned())
            );
            // #2 failed the job #1 failed: #1's holder is responsible for it.
            let responsible = f.sessions.responsible("demo", 2).unwrap().unwrap();
            assert_eq!(responsible.holder.terminal, 3);
            // A failure leaves its holder responsible.
            f.sessions.pipeline_changed("demo", "dev", 2);
            assert_eq!(f.sessions.claims().len(), 2);
            write_pipeline(&f, 3, "succeeded", &[]);
            f.sessions.pipeline_changed("demo", "dev", 3);
            assert!(f.sessions.claims().is_empty());
        }
    }
}
