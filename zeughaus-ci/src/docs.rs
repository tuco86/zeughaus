//! Tickets and onboarding requests of each repository (see
//! [`zeughaus_link::ci::DocView`]).
//!
//! `<state-dir>/ci/docs/<repo>/<number>.json` holds one document as the
//! view the editor shows, proposal and plan included;
//! `<state-dir>/ci/docs/<repo>/last` holds the last number assigned, so a
//! deleted document's number is never given out again, and
//! `<state-dir>/ci/docs/<repo>/paused`, when present, stops the CI from
//! dispatching the repository's approved tickets. Callers serialize the
//! mutating functions (one lock shared by the `/ci` server and the session
//! dispatcher): numbering relies on it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zeughaus_link::ci::{
    DocKind, DocLink, DocState, DocView, LinkKind, MAX_DOC_BYTES, MAX_DOC_TITLE_BYTES, OnboardSpec,
    Planner, RepoDocsView, normalize_origin, plan_title, runs_on_named,
};
use zeughaus_link::selector::parse_runs_on;

use crate::config::{CiConfig, valid_name};
use crate::files::{read_json, write_atomic, write_json};

/// The most links one ticket carries, and the most tickets one plan
/// includes.
const MAX_LINKS: usize = 32;

/// The longest `source` a document may carry.
const MAX_SOURCE_BYTES: usize = 100;

/// What [`create`] stores.
pub struct NewDoc<'a> {
    pub kind: DocKind,
    pub title: &'a str,
    pub body: &'a str,
    pub source: &'a str,
    pub links: &'a [DocLink],
    pub runs_on: &'a [String],
    /// An onboarding request: the repository it asks for.
    pub onboard: Option<&'a OnboardSpec>,
}

/// What [`write_plan`] stores.
pub struct PlanText<'a> {
    pub title: &'a str,
    pub plan: &'a str,
    pub includes: &'a [u64],
    pub runs_on: &'a [String],
    pub source: &'a str,
    pub planner: Option<Planner>,
}

/// Whether a document of `kind` may move from `from` to `to` on a user's
/// request. The CI's own moves (`ready -> planning -> review`,
/// `approved -> in_progress`, `merged`) are not among them.
pub fn allowed(kind: DocKind, from: DocState, to: DocState) -> bool {
    use DocState::*;
    match kind {
        DocKind::Ticket => matches!(
            (from, to),
            (Open, Ready)
                | (Open, Dismissed)
                | (Ready, Open)
                | (Ready, Dismissed)
                | (Review, Approved)
                | (Review, Open)
                | (Review, Dismissed)
                | (Approved, Review)
                | (Approved, Dismissed)
                | (InProgress, Done)
                | (InProgress, Approved)
        ),
        DocKind::Onboarding => matches!((from, to), (Open, Approved) | (Open, Rejected)),
    }
}

/// The directory of `repo`'s documents. The name comes from a request and is
/// joined into a path, so only a repository of `ci.toml`, or one with a
/// pending onboarding request, gets one.
fn dir(state_dir: &Path, config: &CiConfig, repo: &str) -> Result<PathBuf, String> {
    if !valid_name(repo)
        || !(config.repos.contains_key(repo) || pending_request(state_dir, config, repo).is_some())
    {
        return Err(format!("no repository {repo}"));
    }
    Ok(docs_root(state_dir).join(repo))
}

fn docs_root(state_dir: &Path) -> PathBuf {
    crate::ci_dir(state_dir).join("docs")
}

/// The names under `ci/docs/` that can be repositories.
fn doc_dirs(state_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(docs_root(state_dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| valid_name(name))
        .collect();
    names.sort();
    names
}

fn is_pending(doc: &DocView) -> bool {
    doc.kind == DocKind::Onboarding && matches!(doc.state, DocState::Open | DocState::Approved)
}

/// The open or approved onboarding request of `repo` while it is not in
/// `ci.toml`.
fn pending_request(state_dir: &Path, config: &CiConfig, repo: &str) -> Option<DocView> {
    if !valid_name(repo) || config.repos.contains_key(repo) {
        return None;
    }
    read_all(&docs_root(state_dir).join(repo))
        .into_iter()
        .find(is_pending)
}

/// Every open or approved onboarding request of a repository that is not
/// in `ci.toml`, by repository.
pub fn pending(state_dir: &Path, config: &CiConfig) -> Vec<DocView> {
    doc_dirs(state_dir)
        .iter()
        .filter_map(|repo| pending_request(state_dir, config, repo))
        .collect()
}

/// Every approved onboarding request, whether or not `ci.toml` has its
/// repository already: a run cut short after the reload is finished too.
pub fn approved_onboardings(state_dir: &Path) -> Vec<DocView> {
    doc_dirs(state_dir)
        .iter()
        .flat_map(|repo| read_all(&docs_root(state_dir).join(repo)))
        .filter(|doc| doc.kind == DocKind::Onboarding && doc.state == DocState::Approved)
        .collect()
}

/// Records how the CI's attempt at approved onboarding request `number`
/// went: `done`, or back to `open` with the reason as its note.
pub fn finish_onboarding(
    state_dir: &Path,
    repo: &str,
    number: u64,
    result: Result<(), String>,
    now: u64,
) -> Result<(), String> {
    if !valid_name(repo) {
        return Err(format!("no repository {repo}"));
    }
    let path = path_of(&docs_root(state_dir).join(repo), number);
    let mut doc: DocView = read_json(&path).map_err(|_| missing(repo, number))?;
    if doc.kind != DocKind::Onboarding || doc.state != DocState::Approved {
        return Err(format!(
            "#{number} is no approved onboarding request ({} {})",
            doc.kind.name(),
            doc.state.name()
        ));
    }
    match result {
        Ok(()) => {
            doc.state = DocState::Done;
            doc.note.clear();
        }
        Err(note) => {
            doc.state = DocState::Open;
            doc.note = note;
        }
    }
    doc.changed = now;
    write_json(&path, &doc)
}

fn path_of(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number}.json"))
}

/// Every readable document in `dir`, bodies included, in number order.
fn read_all(dir: &Path) -> Vec<DocView> {
    let Ok(files) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut docs: Vec<DocView> = files
        .flatten()
        .map(|file| file.path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .filter_map(|path| read_json::<DocView>(&path).ok())
        .collect();
    docs.sort_by_key(|doc| doc.number);
    docs
}

/// Every document of `repo` without its proposal, plan and review, in
/// number order; nothing for a repository that is not in `ci.toml`.
pub fn list(state_dir: &Path, config: &CiConfig, repo: &str) -> Vec<DocView> {
    let Ok(dir) = dir(state_dir, config, repo) else {
        return Vec::new();
    };
    let mut docs = read_all(&dir);
    for doc in &mut docs {
        doc.body.clear();
        doc.plan_text.clear();
        doc.feedback.clear();
    }
    docs
}

/// Document `number` of `repo` with its body.
pub fn load(state_dir: &Path, config: &CiConfig, repo: &str, number: u64) -> Option<DocView> {
    let dir = dir(state_dir, config, repo).ok()?;
    read_json(&path_of(&dir, number)).ok()
}

fn missing(repo: &str, number: u64) -> String {
    format!("no document #{number} in {repo}")
}

/// The last number given out in `dir`: the `last` file, or the highest
/// stored number should that file be gone.
fn last_number(dir: &Path) -> u64 {
    let recorded = std::fs::read_to_string(dir.join("last"))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let stored = read_all(dir).last().map_or(0, |doc| doc.number);
    recorded.max(stored)
}

fn valid_source(source: &str) -> bool {
    (1..=MAX_SOURCE_BYTES).contains(&source.len())
        && source
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The trimmed title, or why it is refused.
fn checked_title(title: &str) -> Result<&str, String> {
    let title = title.trim();
    if title.is_empty() || title.len() > MAX_DOC_TITLE_BYTES || title.contains(['\n', '\r']) {
        return Err(format!(
            "a title is one line of 1 to {MAX_DOC_TITLE_BYTES} bytes"
        ));
    }
    Ok(title)
}

fn check_body(body: &str) -> Result<(), String> {
    if body.is_empty() || body.len() > MAX_DOC_BYTES {
        return Err(format!("a body is 1 to {MAX_DOC_BYTES} bytes"));
    }
    Ok(())
}

/// `links` of ticket `number` without duplicates, or why they are refused:
/// too many, a link to itself, a target that is no ticket of `repo`, or a
/// dependency that closes a cycle through the stored tickets.
fn check_links(
    dir: &Path,
    repo: &str,
    number: u64,
    links: &[DocLink],
) -> Result<Vec<DocLink>, String> {
    let mut clean: Vec<DocLink> = Vec::with_capacity(links.len());
    for link in links {
        if !clean.contains(link) {
            clean.push(link.clone());
        }
    }
    if clean.len() > MAX_LINKS {
        return Err(format!("a ticket has at most {MAX_LINKS} links"));
    }
    let stored = read_all(dir);
    for link in &clean {
        if link.number == number {
            return Err(format!("#{number} cannot link to itself"));
        }
        match stored.iter().find(|doc| doc.number == link.number) {
            None => return Err(format!("no ticket #{} in {repo}", link.number)),
            Some(doc) if doc.kind != DocKind::Ticket => {
                return Err(format!("#{} is no ticket", link.number));
            }
            Some(_) => {}
        }
    }
    // Who each ticket depends on, with the new set in place of `number`'s.
    let depends = |of: u64| -> Vec<u64> {
        let links = if of == number {
            &clean
        } else {
            match stored.iter().find(|doc| doc.number == of) {
                Some(doc) => &doc.links,
                None => return Vec::new(),
            }
        };
        links
            .iter()
            .filter(|link| link.kind == LinkKind::DependsOn)
            .map(|link| link.number)
            .collect()
    };
    for link in clean.iter().filter(|link| link.kind == LinkKind::DependsOn) {
        if let Some(path) = path_to(link.number, number, &depends) {
            let through = &path[1..path.len() - 1];
            if through.is_empty() {
                return Err(format!(
                    "#{number} depends on #{}, which depends on #{number}",
                    link.number
                ));
            }
            let names: Vec<String> = through.iter().map(|n| format!("#{n}")).collect();
            return Err(format!(
                "a dependency on #{} would close a cycle through {}",
                link.number,
                names.join(", ")
            ));
        }
    }
    Ok(clean)
}

/// A dependency path from `from` to `to`, both included, by a depth-first
/// walk over `depends`; `None` when `to` is not reached.
fn path_to(from: u64, to: u64, depends: &dyn Fn(u64) -> Vec<u64>) -> Option<Vec<u64>> {
    let mut seen = BTreeSet::new();
    let mut path = vec![from];
    fn walk(
        at: u64,
        to: u64,
        depends: &dyn Fn(u64) -> Vec<u64>,
        seen: &mut BTreeSet<u64>,
        path: &mut Vec<u64>,
    ) -> bool {
        if at == to {
            return true;
        }
        if !seen.insert(at) {
            return false;
        }
        for next in depends(at) {
            path.push(next);
            if walk(next, to, depends, seen, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    walk(from, to, depends, &mut seen, &mut path).then_some(path)
}

/// Stores a new document and answers its number. A ticket and an
/// onboarding request start `open`. A refusal writes nothing.
pub fn create(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    new: NewDoc<'_>,
    now: u64,
) -> Result<u64, String> {
    let onboard = match (new.kind, new.onboard) {
        (DocKind::Onboarding, Some(spec)) => Some(crate::onboard::check_request(
            state_dir, config, repo, spec,
        )?),
        (DocKind::Onboarding, None) => {
            return Err("an onboarding request names the repository it asks for".to_owned());
        }
        (_, Some(_)) => {
            return Err("only an onboarding request names a repository to onboard".to_owned());
        }
        (_, None) => None,
    };
    // A request's repository has no directory of its own yet: `dir` would
    // refuse the name, which `check_request` has vetted.
    let dir = if onboard.is_some() {
        docs_root(state_dir).join(repo)
    } else {
        dir(state_dir, config, repo)?
    };
    let title = checked_title(new.title)?;
    check_body(new.body)?;
    if !valid_source(new.source) {
        return Err(format!(
            "a source is 1 to {MAX_SOURCE_BYTES} of A-Z a-z 0-9 . _ -"
        ));
    }
    parse_runs_on(new.runs_on)?;
    if !new.links.is_empty() {
        if new.kind != DocKind::Ticket {
            return Err("only a ticket has links".to_owned());
        }
        if !config.repos.contains_key(repo) {
            return Err(format!(
                "{repo} is not in ci.toml yet; links wait for its onboarding"
            ));
        }
    }
    let number = last_number(&dir) + 1;
    let links = check_links(&dir, repo, number, new.links)?;
    let doc = DocView {
        repo: repo.to_owned(),
        number,
        kind: new.kind,
        title: title.to_owned(),
        state: DocState::Open,
        source: new.source.to_owned(),
        created: now,
        changed: now,
        approved: None,
        released: None,
        links,
        merged: Vec::new(),
        into: None,
        runs_on: new.runs_on.to_vec(),
        placement: Vec::new(),
        body: new.body.to_owned(),
        plan_text: String::new(),
        revision: 0,
        planner: None,
        feedback: String::new(),
        onboard,
        note: String::new(),
    };
    write_json(&path_of(&dir, number), &doc)?;
    write_atomic(&dir.join("last"), number.to_string().as_bytes())?;
    Ok(number)
}

/// The ticket at `path`, or that it is none.
fn read_ticket(path: &Path, repo: &str, number: u64) -> Result<DocView, String> {
    let doc: DocView = read_json(path).map_err(|_| missing(repo, number))?;
    if doc.kind != DocKind::Ticket {
        return Err(format!("#{number} is no ticket"));
    }
    Ok(doc)
}

/// Replaces the title and proposal of an open or ready ticket.
pub fn edit(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    title: &str,
    body: &str,
    now: u64,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if !matches!(doc.state, DocState::Open | DocState::Ready) {
        return Err(format!(
            "#{number} is {}; its text is fixed",
            doc.state.name()
        ));
    }
    let title = checked_title(title)?;
    check_body(body)?;
    doc.title = title.to_owned();
    doc.body = body.to_owned();
    doc.changed = now;
    write_json(&path, &doc)
}

/// Replaces the outgoing links of a ticket that is not finished.
pub fn set_links(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    links: Vec<DocLink>,
    now: u64,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if matches!(
        doc.state,
        DocState::Done | DocState::Dismissed | DocState::Merged
    ) {
        return Err(format!(
            "#{number} is {}; its links are fixed",
            doc.state.name()
        ));
    }
    doc.links = check_links(&dir, repo, number, &links)?;
    doc.changed = now;
    write_json(&path, &doc)
}

/// What [`file_request`] did with a job's session request.
#[derive(Debug, PartialEq, Eq)]
pub enum Filed {
    /// Stored as this approved ticket.
    Queued(u64),
    /// This ticket in review, approved or in progress came from the same
    /// job with the same plan; nothing was stored.
    StillOpen(u64),
}

/// Files the session request job `job` of `repo` left as an approved ticket
/// with source `job.<job>`, titled by its first `# ` line and placed by its
/// `Runs on:` line. Refused for a repository without `agent_sessions`.
pub fn file_request(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    job: &str,
    body: &str,
    now: u64,
) -> Result<Filed, String> {
    if !config.repos.get(repo).is_some_and(|r| r.agent_sessions) {
        return Err(format!(
            "{repo} does not allow agent sessions (agent_sessions in ci.toml)"
        ));
    }
    let source = format!("job.{job}");
    let dir = dir(state_dir, config, repo)?;
    if let Some(open) = read_all(&dir).iter().find(|doc| {
        doc.kind == DocKind::Ticket
            && doc.source == source
            && matches!(
                doc.state,
                DocState::Review | DocState::Approved | DocState::InProgress
            )
            && doc.plan_text == body
    }) {
        return Ok(Filed::StillOpen(open.number));
    }
    let runs_on = runs_on_named(body)?;
    let title = plan_title(body);
    let number = write_plan(
        state_dir,
        config,
        repo,
        None,
        PlanText {
            title: &title,
            plan: body,
            includes: &[],
            runs_on: &runs_on,
            source: &source,
            planner: None,
        },
        now,
    )?;
    set_state(state_dir, config, repo, number, DocState::Approved, now)?;
    Ok(Filed::Queued(number))
}

/// The tickets merged into `number`: back to `open`, or `done` with it.
fn settle_merged(
    dir: &Path,
    number: u64,
    merged: &[u64],
    done: bool,
    now: u64,
) -> Result<(), String> {
    for &other in merged {
        let path = path_of(dir, other);
        let Ok(mut doc) = read_json::<DocView>(&path) else {
            continue;
        };
        if doc.kind != DocKind::Ticket || doc.state != DocState::Merged || doc.into != Some(number)
        {
            continue;
        }
        if done {
            doc.state = DocState::Done;
        } else {
            doc.state = DocState::Open;
            doc.into = None;
        }
        doc.changed = now;
        write_json(&path, &doc)?;
    }
    Ok(())
}

/// Moves document `number` to `to`, where [`allowed`] lets it. Releasing a
/// ticket stamps `released`, which orders the planning queue; approving it
/// stamps `approved`, which orders the masters' queue, and a requeued
/// ticket keeps its stamp and with it its place. The tickets a plan
/// includes follow it: `done` with it, back to `open` when it is dismissed
/// or its plan discarded.
pub fn set_state(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    to: DocState,
    now: u64,
) -> Result<(), String> {
    if matches!(
        to,
        DocState::InProgress | DocState::Planning | DocState::Merged
    ) {
        return Err(format!("only the CI moves a ticket to {}", to.name()));
    }
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc: DocView = read_json(&path).map_err(|_| missing(repo, number))?;
    if !allowed(doc.kind, doc.state, to) {
        return Err(format!(
            "#{number} is {}; it cannot become {}",
            doc.state.name(),
            to.name()
        ));
    }
    let from = doc.state;
    match (from, to) {
        (DocState::Open, DocState::Ready) => {
            doc.released = Some(now);
            doc.note.clear();
        }
        (DocState::Review, DocState::Approved) => {
            doc.approved = Some(now);
            doc.feedback.clear();
        }
        (DocState::Review, DocState::Open) => {
            doc.plan_text.clear();
            doc.planner = None;
            doc.feedback.clear();
        }
        _ => {}
    }
    let reopen = doc.kind == DocKind::Ticket
        && (to == DocState::Dismissed || (from, to) == (DocState::Review, DocState::Open));
    let close = doc.kind == DocKind::Ticket && to == DocState::Done;
    let merged = doc.merged.clone();
    if reopen {
        doc.merged.clear();
    }
    doc.state = to;
    doc.changed = now;
    write_json(&path, &doc)?;
    if reopen || close {
        settle_merged(&dir, number, &merged, close, now)?;
    }
    Ok(())
}

/// Stores a plan on ticket `number`, which a plan session works on, or as a
/// new ticket without one, and answers the ticket's number; either is then
/// in `review`. The tickets the plan includes become `merged` into it, and
/// those an earlier revision included and this one does not go back to
/// `open`. A refusal writes nothing.
pub fn write_plan(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: Option<u64>,
    plan: PlanText<'_>,
    now: u64,
) -> Result<u64, String> {
    if !config.repos.contains_key(repo) {
        return Err(format!(
            "{repo} is not in ci.toml yet; plans wait for its onboarding"
        ));
    }
    let dir = dir(state_dir, config, repo)?;
    if plan.plan.is_empty() || plan.plan.len() > MAX_DOC_BYTES {
        return Err(format!("a plan is 1 to {MAX_DOC_BYTES} bytes"));
    }
    let title = checked_title(plan.title)?;
    if !valid_source(plan.source) {
        return Err(format!(
            "a source is 1 to {MAX_SOURCE_BYTES} of A-Z a-z 0-9 . _ -"
        ));
    }
    parse_runs_on(plan.runs_on)?;
    if plan.includes.len() > MAX_LINKS {
        return Err(format!("a plan includes at most {MAX_LINKS} tickets"));
    }
    let fresh = number.is_none();
    let (number, mut doc) = match number {
        Some(n) => {
            let doc = read_ticket(&path_of(&dir, n), repo, n)?;
            if doc.state != DocState::Planning {
                return Err(format!(
                    "#{n} is {}; a plan is written for a ticket in planning",
                    doc.state.name()
                ));
            }
            (n, doc)
        }
        None => {
            let n = last_number(&dir) + 1;
            let doc = DocView {
                repo: repo.to_owned(),
                number: n,
                kind: DocKind::Ticket,
                title: title.to_owned(),
                state: DocState::Review,
                source: plan.source.to_owned(),
                created: now,
                changed: now,
                approved: None,
                released: None,
                links: Vec::new(),
                merged: Vec::new(),
                into: None,
                runs_on: Vec::new(),
                placement: Vec::new(),
                body: String::new(),
                plan_text: String::new(),
                revision: 0,
                planner: None,
                feedback: String::new(),
                onboard: None,
                note: String::new(),
            };
            (n, doc)
        }
    };
    let mut includes: Vec<u64> = Vec::with_capacity(plan.includes.len());
    let mut taken: Vec<DocView> = Vec::new();
    for &other in plan.includes {
        if other == number || includes.contains(&other) {
            continue;
        }
        let included = read_json::<DocView>(&path_of(&dir, other))
            .map_err(|_| format!("no ticket #{other} in {repo}"))?;
        if included.kind != DocKind::Ticket {
            return Err(format!("#{other} is no ticket"));
        }
        let fits = matches!(included.state, DocState::Open | DocState::Ready)
            || (included.state == DocState::Merged && included.into == Some(number));
        if !fits {
            return Err(format!(
                "#{other} is {}; a plan includes open or ready tickets only",
                included.state.name()
            ));
        }
        includes.push(other);
        taken.push(included);
    }
    let dropped: Vec<u64> = doc
        .merged
        .iter()
        .copied()
        .filter(|n| !includes.contains(n))
        .collect();
    doc.plan_text = plan.plan.to_owned();
    doc.revision += 1;
    doc.planner = plan.planner;
    doc.feedback.clear();
    doc.note.clear();
    if !plan.runs_on.is_empty() {
        doc.runs_on = plan.runs_on.to_vec();
    }
    doc.merged = includes;
    doc.state = DocState::Review;
    doc.changed = now;
    write_json(&path_of(&dir, number), &doc)?;
    if fresh {
        write_atomic(&dir.join("last"), number.to_string().as_bytes())?;
    }
    settle_merged(&dir, number, &dropped, false, now)?;
    for mut included in taken {
        if included.state == DocState::Merged {
            continue;
        }
        included.state = DocState::Merged;
        included.into = Some(number);
        included.changed = now;
        write_json(&path_of(&dir, included.number), &included)?;
    }
    Ok(number)
}

/// Sends the user's review back to the plan session of a ticket in review:
/// the ticket is `ready` again, at its old place in the planning queue.
pub fn refine(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    feedback: &str,
    now: u64,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if doc.state != DocState::Review {
        return Err(format!(
            "#{number} is {}; only a plan in review is refined",
            doc.state.name()
        ));
    }
    if feedback.is_empty() || feedback.len() > MAX_DOC_BYTES {
        return Err(format!("a review is 1 to {MAX_DOC_BYTES} bytes"));
    }
    doc.feedback = feedback.to_owned();
    doc.state = DocState::Ready;
    doc.released.get_or_insert(now);
    doc.changed = now;
    write_json(&path, &doc)
}

/// Ready ticket `number` gets a plan session: it becomes `planning` and
/// comes back whole.
pub fn start_planning(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    now: u64,
) -> Result<DocView, String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if doc.state != DocState::Ready {
        return Err(format!(
            "#{number} is {}; only a ready ticket is planned",
            doc.state.name()
        ));
    }
    doc.state = DocState::Planning;
    doc.changed = now;
    write_json(&path, &doc)?;
    Ok(doc)
}

/// The plan session of ticket `number` ended. A ticket still in `planning`
/// got no new plan: back to `review` with its last plan, else to `open`,
/// with `note` saying why. Answers whether the ticket changed.
pub fn planning_ended(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    note: &str,
    now: u64,
) -> Result<bool, String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if doc.state != DocState::Planning {
        return Ok(false);
    }
    doc.state = if doc.plan_text.is_empty() {
        DocState::Open
    } else {
        DocState::Review
    };
    doc.note = note.to_owned();
    doc.changed = now;
    write_json(&path, &doc)?;
    Ok(true)
}

/// Every repository's ready tickets, without their texts, in the order
/// plan sessions take them: by release, then number.
pub fn ready(state_dir: &Path, config: &CiConfig) -> Vec<DocView> {
    let mut tickets: Vec<DocView> = config
        .repos
        .keys()
        .flat_map(|repo| list(state_dir, config, repo))
        .filter(|doc| doc.kind == DocKind::Ticket && doc.state == DocState::Ready)
        .collect();
    tickets.sort_by_key(|doc| (doc.released.unwrap_or(0), doc.number));
    tickets
}

/// Deletes a document that is finished with: `done`, `dismissed`,
/// `merged` or `rejected`.
pub fn delete(state_dir: &Path, config: &CiConfig, repo: &str, number: u64) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let doc: DocView = read_json(&path).map_err(|_| missing(repo, number))?;
    if !matches!(
        doc.state,
        DocState::Done | DocState::Dismissed | DocState::Merged | DocState::Rejected
    ) {
        return Err(format!(
            "#{number} is {}; only a finished document is deleted",
            doc.state.name()
        ));
    }
    // `last` keeps the number, so it is not given out again.
    if !dir.join("last").exists() {
        write_atomic(&dir.join("last"), last_number(&dir).to_string().as_bytes())?;
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot delete {}: {e}", path.display()))
}

/// Sets the `Runs on` of a ticket that is open, ready, in review or
/// approved.
pub fn set_runs_on(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    runs_on: Vec<String>,
    now: u64,
) -> Result<(), String> {
    parse_runs_on(&runs_on)?;
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc: DocView = read_json(&path).map_err(|_| missing(repo, number))?;
    let open = doc.kind == DocKind::Ticket
        && matches!(
            doc.state,
            DocState::Open | DocState::Ready | DocState::Review | DocState::Approved
        );
    if !open {
        return Err(format!(
            "#{number} is {}; its Runs on is fixed",
            doc.state.name()
        ));
    }
    doc.runs_on = runs_on;
    doc.changed = now;
    write_json(&path, &doc)
}

/// Approved ticket `number` becomes `in_progress` on the runners of
/// `placement` (the host first) and comes back with its plan.
pub fn start(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    placement: Vec<String>,
    now: u64,
) -> Result<DocView, String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc: DocView = read_json(&path).map_err(|_| missing(repo, number))?;
    if doc.kind != DocKind::Ticket || doc.state != DocState::Approved {
        return Err(format!(
            "#{number} is {}; only an approved ticket starts",
            doc.state.name()
        ));
    }
    doc.state = DocState::InProgress;
    doc.placement = placement;
    doc.changed = now;
    write_json(&path, &doc)?;
    Ok(doc)
}

/// Every repository's approved tickets, without their texts, in the order
/// they are dispatched: by approval, then number.
pub fn queue(state_dir: &Path, config: &CiConfig) -> Vec<DocView> {
    let mut tickets: Vec<DocView> = config
        .repos
        .keys()
        .flat_map(|repo| list(state_dir, config, repo))
        .filter(|doc| doc.kind == DocKind::Ticket && doc.state == DocState::Approved)
        .collect();
    tickets.sort_by_key(|doc| (doc.approved.unwrap_or(0), doc.number));
    tickets
}

/// Whether the CI dispatches none of `repo`'s approved tickets.
pub fn paused(state_dir: &Path, config: &CiConfig, repo: &str) -> bool {
    dir(state_dir, config, repo).is_ok_and(|dir| dir.join("paused").exists())
}

pub fn set_paused(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    paused: bool,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let file = dir.join("paused");
    if paused {
        write_atomic(&file, b"")
    } else {
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("cannot remove {}: {e}", file.display()))
            }
            _ => Ok(()),
        }
    }
}

fn count(docs: &[DocView], states: &[DocState]) -> u32 {
    docs.iter()
        .filter(|doc| doc.kind == DocKind::Ticket && states.contains(&doc.state))
        .count() as u32
}

/// One line per repository of `ci.toml`, then per repository with a
/// pending onboarding request, all in name order: what waits for whom.
pub fn summary(state_dir: &Path, config: &CiConfig) -> Vec<RepoDocsView> {
    let mut views: Vec<RepoDocsView> = config
        .repos
        .iter()
        .map(|(name, repo)| {
            let docs = list(state_dir, config, name);
            RepoDocsView {
                repo: name.clone(),
                origin: normalize_origin(&repo.url),
                default_branch: repo.default_branch.clone(),
                awaiting: count(&docs, &[DocState::Review]),
                queued: count(&docs, &[DocState::Approved]),
                in_progress: docs
                    .iter()
                    .filter(|doc| doc.kind == DocKind::Ticket && doc.state == DocState::InProgress)
                    .map(|doc| doc.number)
                    .collect(),
                paused: paused(state_dir, config, name),
                open: count(&docs, &[DocState::Open]),
                planning: count(&docs, &[DocState::Ready, DocState::Planning]),
                onboarding: None,
            }
        })
        .collect();
    for request in pending(state_dir, config) {
        let Some(spec) = &request.onboard else {
            continue;
        };
        let docs = list(state_dir, config, &request.repo);
        let open = request.state == DocState::Open;
        views.push(RepoDocsView {
            repo: request.repo.clone(),
            origin: normalize_origin(&spec.url),
            default_branch: spec.default_branch.clone(),
            awaiting: u32::from(open),
            queued: 0,
            in_progress: if open {
                Vec::new()
            } else {
                vec![request.number]
            },
            paused: false,
            open: count(&docs, &[DocState::Open]),
            planning: 0,
            onboarding: Some(request.number),
        });
    }
    views.sort_by(|a, b| a.repo.cmp(&b.repo));
    views
}

/// Rewrites every stored proposal and plan from before tickets as a
/// ticket, and answers how many documents it changed. A file that does
/// not parse is left alone.
pub fn migrate(state_dir: &Path) -> Result<usize, String> {
    let mut changed = 0;
    for repo in doc_dirs(state_dir) {
        let dir = docs_root(state_dir).join(&repo);
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in files.flatten().map(|file| file.path()) {
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(mut doc) = read_json::<serde_json::Value>(&path) else {
                continue;
            };
            if migrate_doc(&mut doc) {
                write_json(&path, &doc)?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// One stored document from before tickets as a ticket: a proposal keeps
/// its text and a `planned` one is `merged` into its plan; a plan becomes a
/// ticket whose plan is its body and which includes its proposals. Answers
/// whether it changed anything.
fn migrate_doc(doc: &mut serde_json::Value) -> bool {
    use serde_json::Value;
    let Some(object) = doc.as_object_mut() else {
        return false;
    };
    let kind = object.get("kind").and_then(Value::as_str).unwrap_or("");
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match kind {
        "proposal" => {
            let plan = object.get("plan").and_then(Value::as_u64);
            let state = match (state.as_str(), plan) {
                ("planned", Some(plan)) => {
                    object.insert("into".to_owned(), Value::from(plan));
                    "merged"
                }
                ("planned", None) => "done",
                (other, _) => match other {
                    "open" | "dismissed" | "done" => other,
                    _ => "open",
                },
            }
            .to_owned();
            object.insert("state".to_owned(), Value::from(state));
        }
        "plan" => {
            let body = object.remove("body").unwrap_or(Value::from(""));
            object.insert("plan_text".to_owned(), body);
            object.insert("body".to_owned(), Value::from(""));
            let merged = object
                .get("proposals")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            object.insert("merged".to_owned(), merged);
            object.insert("revision".to_owned(), Value::from(1));
            let state = match state.as_str() {
                "saved" => "review",
                "approved" => "approved",
                "in_progress" => "in_progress",
                "done" => "done",
                _ => "dismissed",
            };
            object.insert("state".to_owned(), Value::from(state));
        }
        _ => return false,
    }
    object.insert("kind".to_owned(), Value::from("ticket"));
    object.remove("proposals");
    object.remove("plan");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use DocState::*;

    #[test]
    fn only_the_transitions_of_the_table_are_allowed() {
        let ticket = |from, to| allowed(DocKind::Ticket, from, to);
        for (from, to) in [
            (Open, Ready),
            (Open, Dismissed),
            (Ready, Open),
            (Ready, Dismissed),
            (Review, Approved),
            (Review, Open),
            (Review, Dismissed),
            (Approved, Review),
            (Approved, Dismissed),
            (InProgress, Done),
            (InProgress, Approved),
        ] {
            assert!(ticket(from, to), "{from:?} -> {to:?}");
        }
        for (from, to) in [
            (Open, Review),
            (Open, Approved),
            (Ready, Planning),
            (Planning, Review),
            (Planning, Open),
            (Approved, InProgress),
            (Review, Done),
            (Done, Open),
            (Merged, Open),
            (Dismissed, Open),
            (Open, Rejected),
        ] {
            assert!(!ticket(from, to), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn an_onboarding_request_is_approved_or_rejected_and_the_ci_finishes_it() {
        let onboarding = |from, to| allowed(DocKind::Onboarding, from, to);
        assert!(onboarding(Open, Approved));
        assert!(onboarding(Open, Rejected));
        assert!(!onboarding(Approved, Done));
        assert!(!onboarding(Open, Done));
        assert!(!onboarding(Approved, Open));
        assert!(!onboarding(Approved, Rejected));
    }

    fn migrated(text: &str) -> Option<serde_json::Value> {
        let mut doc: serde_json::Value = serde_json::from_str(text).unwrap();
        migrate_doc(&mut doc).then_some(doc)
    }

    const HEAD: &str = r#""repo":"demo","title":"T","source":"s","created":1,"changed":2"#;

    #[test]
    fn a_planned_proposal_becomes_merged_into_its_plan() {
        let doc = migrated(&format!(
            r#"{{{HEAD},"number":1,"kind":"proposal","state":"planned","plan":2,"proposals":[],"body":"B"}}"#
        ))
        .unwrap();
        let view: DocView = serde_json::from_value(doc.clone()).unwrap();
        assert_eq!(
            (view.kind, view.state, view.into, view.body.as_str()),
            (DocKind::Ticket, Merged, Some(2), "B")
        );
        assert!(doc.get("plan").is_none() && doc.get("proposals").is_none());

        let orphan = migrated(&format!(
            r#"{{{HEAD},"number":1,"kind":"proposal","state":"planned"}}"#
        ))
        .unwrap();
        let view: DocView = serde_json::from_value(orphan).unwrap();
        assert_eq!((view.state, view.into), (Done, None));
    }

    #[test]
    fn a_saved_plan_becomes_a_ticket_in_review_that_includes_its_proposals() {
        let doc = migrated(&format!(
            r##"{{{HEAD},"number":2,"kind":"plan","state":"saved","proposals":[1,3],"plan":null,"body":"# Old plan\n"}}"##
        ))
        .unwrap();
        let view: DocView = serde_json::from_value(doc).unwrap();
        assert_eq!(
            (
                view.kind,
                view.state,
                view.merged,
                view.revision,
                view.body.as_str(),
                view.plan_text.as_str()
            ),
            (DocKind::Ticket, Review, vec![1, 3], 1, "", "# Old plan\n")
        );
        let rejected = migrated(&format!(
            r#"{{{HEAD},"number":2,"kind":"plan","state":"rejected","body":"x"}}"#
        ))
        .unwrap();
        assert_eq!(
            serde_json::from_value::<DocView>(rejected).unwrap().state,
            Dismissed
        );
    }

    #[test]
    fn an_onboarding_request_and_a_ticket_are_not_migrated() {
        for kind in ["onboarding", "ticket"] {
            assert!(
                migrated(&format!(
                    r#"{{{HEAD},"number":1,"kind":"{kind}","state":"open","body":"b"}}"#
                ))
                .is_none()
            );
        }
    }

    /// Tests that touch the filesystem: the integration tier.
    mod io {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("zci-docs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the scratch directory");
            dir
        }

        fn config() -> CiConfig {
            CiConfig::parse(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n\
                 [repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\n",
            )
            .expect("a valid ci.toml")
        }

        fn new<'a>(title: &'a str, links: &'a [DocLink]) -> NewDoc<'a> {
            NewDoc {
                kind: DocKind::Ticket,
                title,
                body: "body",
                source: "weida",
                links,
                runs_on: &[],
                onboard: None,
            }
        }

        fn plan<'a>(includes: &'a [u64]) -> PlanText<'a> {
            PlanText {
                title: "Plan",
                plan: "# Plan\n",
                includes,
                runs_on: &[],
                source: "demo",
                planner: None,
            }
        }

        fn depends(numbers: &[u64]) -> Vec<DocLink> {
            numbers
                .iter()
                .map(|&number| DocLink {
                    kind: LinkKind::DependsOn,
                    number,
                })
                .collect()
        }

        fn state_of(state: &Path, config: &CiConfig, number: u64) -> DocState {
            load(state, config, "demo", number).expect("stored").state
        }

        /// Ticket `number` from open through a plan session to review.
        fn planned(state: &Path, config: &CiConfig, number: u64, includes: &[u64], now: u64) {
            set_state(state, config, "demo", number, Ready, now).unwrap();
            start_planning(state, config, "demo", number, now).unwrap();
            write_plan(state, config, "demo", Some(number), plan(includes), now).unwrap();
        }

        #[test]
        fn numbers_rise_across_kinds_and_a_deleted_one_is_not_reused() {
            let state = scratch("numbers");
            let config = config();
            let a = create(&state, &config, "demo", new("a", &[]), 1);
            let b = write_plan(&state, &config, "demo", None, plan(&[]), 2);
            assert_eq!((a, b), (Ok(1), Ok(2)));
            set_state(&state, &config, "demo", 2, Dismissed, 3).expect("dismiss");
            delete(&state, &config, "demo", 2).expect("delete");
            let c = create(&state, &config, "demo", new("c", &[]), 4);
            assert_eq!(c, Ok(3));
            let numbers: Vec<u64> = list(&state, &config, "demo")
                .iter()
                .map(|doc| doc.number)
                .collect();
            assert_eq!(numbers, vec![1, 3]);
            assert!(
                list(&state, &config, "demo")
                    .iter()
                    .all(|d| d.body.is_empty())
            );
            assert_eq!(
                load(&state, &config, "demo", 3).expect("stored").body,
                "body"
            );
        }

        #[test]
        fn a_plan_merges_the_tickets_it_includes_and_a_revision_lets_one_go() {
            let state = scratch("includes");
            let config = config();
            for title in ["main", "a", "b", "c"] {
                create(&state, &config, "demo", new(title, &[]), 1).unwrap();
            }
            set_state(&state, &config, "demo", 4, Dismissed, 1).unwrap();
            set_state(&state, &config, "demo", 1, Ready, 2).unwrap();
            start_planning(&state, &config, "demo", 1, 2).unwrap();
            assert_eq!(
                write_plan(&state, &config, "demo", Some(1), plan(&[2, 4]), 3),
                Err("#4 is dismissed; a plan includes open or ready tickets only".to_owned())
            );
            // Nothing written by the refusal.
            assert_eq!(state_of(&state, &config, 2), Open);
            assert_eq!(state_of(&state, &config, 1), Planning);

            let planner = Planner {
                runner: "atik".to_owned(),
                omp_session: "/s/1.jsonl".to_owned(),
            };
            let first = PlanText {
                planner: Some(planner.clone()),
                ..plan(&[2, 3, 1])
            };
            assert_eq!(
                write_plan(&state, &config, "demo", Some(1), first, 4),
                Ok(1)
            );
            let ticket = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(
                (ticket.state, ticket.revision, ticket.merged.clone()),
                (Review, 1, vec![2, 3])
            );
            assert_eq!(ticket.planner, Some(planner));
            for number in [2, 3] {
                let merged = load(&state, &config, "demo", number).unwrap();
                assert_eq!((merged.state, merged.into), (Merged, Some(1)));
            }

            // Refine, plan again without #3: it goes back to open.
            refine(&state, &config, "demo", 1, "Overall:\nless", 5).unwrap();
            assert_eq!(state_of(&state, &config, 1), Ready);
            start_planning(&state, &config, "demo", 1, 6).unwrap();
            write_plan(&state, &config, "demo", Some(1), plan(&[2]), 7).unwrap();
            let ticket = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(
                (ticket.revision, ticket.merged, ticket.feedback.as_str()),
                (2, vec![2], "")
            );
            assert_eq!(state_of(&state, &config, 2), Merged);
            let free = load(&state, &config, "demo", 3).unwrap();
            assert_eq!((free.state, free.into), (Open, None));
        }

        #[test]
        fn included_tickets_close_with_their_ticket_or_reopen_when_its_plan_goes() {
            let state = scratch("follow");
            let config = config();
            for title in ["a", "b", "c", "d"] {
                create(&state, &config, "demo", new(title, &[]), 1).unwrap();
            }
            planned(&state, &config, 1, &[2], 2);
            // Discarding the plan reopens what it included.
            set_state(&state, &config, "demo", 1, Open, 3).unwrap();
            let discarded = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(
                (discarded.plan_text.as_str(), discarded.merged.len()),
                ("", 0)
            );
            assert_eq!(discarded.revision, 1);
            let reopened = load(&state, &config, "demo", 2).unwrap();
            assert_eq!((reopened.state, reopened.into), (Open, None));

            planned(&state, &config, 1, &[2], 4);
            planned(&state, &config, 3, &[4], 4);
            set_state(&state, &config, "demo", 1, Approved, 5).unwrap();
            start(&state, &config, "demo", 1, vec!["x".to_owned()], 6).unwrap();
            set_state(&state, &config, "demo", 1, Done, 7).unwrap();
            assert_eq!(state_of(&state, &config, 2), Done);
            // Another ticket's merged ticket stays.
            assert_eq!(state_of(&state, &config, 4), Merged);
            set_state(&state, &config, "demo", 3, Dismissed, 8).unwrap();
            assert_eq!(state_of(&state, &config, 4), Open);
        }

        #[test]
        fn a_dependency_that_closes_a_cycle_is_refused() {
            let state = scratch("cycles");
            let config = config();
            create(&state, &config, "demo", new("a", &[]), 1).unwrap();
            create(&state, &config, "demo", new("b", &depends(&[1])), 1).unwrap();
            create(&state, &config, "demo", new("c", &depends(&[2])), 1).unwrap();
            assert_eq!(
                set_links(&state, &config, "demo", 1, depends(&[2]), 2),
                Err("#1 depends on #2, which depends on #1".to_owned())
            );
            assert_eq!(
                set_links(&state, &config, "demo", 1, depends(&[3]), 2),
                Err("a dependency on #3 would close a cycle through #2".to_owned())
            );
            assert_eq!(
                set_links(&state, &config, "demo", 1, depends(&[1]), 2),
                Err("#1 cannot link to itself".to_owned())
            );
            assert_eq!(
                set_links(&state, &config, "demo", 1, depends(&[9]), 2),
                Err("no ticket #9 in demo".to_owned())
            );
            // A related link closes no cycle, and duplicates collapse.
            let related = DocLink {
                kind: LinkKind::Related,
                number: 3,
            };
            set_links(
                &state,
                &config,
                "demo",
                1,
                vec![related.clone(), related.clone()],
                3,
            )
            .unwrap();
            assert_eq!(
                load(&state, &config, "demo", 1).unwrap().links,
                vec![related]
            );
        }

        #[test]
        fn a_planning_ticket_without_a_plan_goes_back_with_a_note() {
            let state = scratch("planning-ended");
            let config = config();
            create(&state, &config, "demo", new("a", &[]), 1).unwrap();
            set_state(&state, &config, "demo", 1, Ready, 2).unwrap();
            assert_eq!(
                start_planning(&state, &config, "demo", 1, 3).unwrap().state,
                Planning
            );
            assert!(start_planning(&state, &config, "demo", 1, 3).is_err());
            assert_eq!(
                planning_ended(&state, &config, "demo", 1, "no plan", 4),
                Ok(true)
            );
            let back = load(&state, &config, "demo", 1).unwrap();
            assert_eq!((back.state, back.note.as_str()), (Open, "no plan"));
            assert_eq!(
                planning_ended(&state, &config, "demo", 1, "no plan", 5),
                Ok(false)
            );
            // Releasing again clears the note; a revision keeps its plan.
            planned(&state, &config, 1, &[], 6);
            assert_eq!(load(&state, &config, "demo", 1).unwrap().note, "");
            refine(&state, &config, "demo", 1, "redo", 7).unwrap();
            start_planning(&state, &config, "demo", 1, 8).unwrap();
            planning_ended(&state, &config, "demo", 1, "ended", 9).unwrap();
            assert_eq!(state_of(&state, &config, 1), Review);
        }

        #[test]
        fn tickets_start_in_approval_order_and_several_run_at_once() {
            let state = scratch("start");
            let config = config();
            for _ in 0..3 {
                write_plan(&state, &config, "demo", None, plan(&[]), 1).unwrap();
            }
            assert!(queue(&state, &config).is_empty());
            // #2 approved before #1: the queue follows approval.
            set_state(&state, &config, "demo", 2, Approved, 10).unwrap();
            set_state(&state, &config, "demo", 1, Approved, 20).unwrap();
            assert_eq!(
                set_state(&state, &config, "demo", 1, InProgress, 21),
                Err("only the CI moves a ticket to in_progress".to_owned())
            );
            let queued: Vec<u64> = queue(&state, &config).iter().map(|d| d.number).collect();
            assert_eq!(queued, vec![2, 1]);

            let first = start(&state, &config, "demo", 2, vec!["atik".to_owned()], 30).unwrap();
            assert_eq!(
                (first.state, first.plan_text.as_str()),
                (InProgress, "# Plan\n")
            );
            let second = start(&state, &config, "demo", 1, vec!["x".to_owned()], 31).unwrap();
            assert_eq!(second.placement, vec!["x".to_owned()]);
            assert_eq!(summary(&state, &config)[0].in_progress, vec![1, 2]);
            assert_eq!(
                start(&state, &config, "demo", 3, Vec::new(), 32),
                Err("#3 is review; only an approved ticket starts".to_owned())
            );
            assert!(queue(&state, &config).is_empty());
        }

        #[test]
        fn runs_on_is_fixed_once_a_ticket_is_in_progress() {
            let state = scratch("runs-on");
            let config = config();
            write_plan(&state, &config, "demo", None, plan(&[]), 1).unwrap();
            let windows = vec!["os=windows".to_owned()];
            set_runs_on(&state, &config, "demo", 1, windows.clone(), 2).unwrap();
            assert_eq!(load(&state, &config, "demo", 1).unwrap().runs_on, windows);
            assert!(set_runs_on(&state, &config, "demo", 1, vec!["Bad".to_owned()], 3).is_err());
            set_state(&state, &config, "demo", 1, Approved, 4).unwrap();
            start(&state, &config, "demo", 1, vec!["w".to_owned()], 5).unwrap();
            assert_eq!(
                set_runs_on(&state, &config, "demo", 1, Vec::new(), 6),
                Err("#1 is in_progress; its Runs on is fixed".to_owned())
            );
        }

        #[test]
        fn a_paused_queue_is_a_file() {
            let state = scratch("paused");
            let config = config();
            assert!(!paused(&state, &config, "demo"));
            set_paused(&state, &config, "demo", true).unwrap();
            assert!(paused(&state, &config, "demo"));
            assert!(summary(&state, &config)[0].paused);
            set_paused(&state, &config, "demo", false).unwrap();
            set_paused(&state, &config, "demo", false).unwrap();
            assert!(!paused(&state, &config, "demo"));
        }

        #[test]
        fn a_repository_outside_ci_toml_reads_and_writes_nothing() {
            let state = scratch("outside");
            let config = config();
            for repo in ["other", "../x"] {
                let made = create(&state, &config, repo, new("a", &[]), 1);
                assert_eq!(made, Err(format!("no repository {repo}")));
                assert!(list(&state, &config, repo).is_empty());
                assert!(start(&state, &config, repo, 1, Vec::new(), 1).is_err());
            }
            assert!(!state.join("ci").exists());
            assert!(!state.join("x").exists());
        }

        #[test]
        fn old_proposals_and_plans_on_disk_become_tickets() {
            let state = scratch("migrate");
            let config = config();
            let dir = docs_root(&state).join("demo");
            std::fs::create_dir_all(&dir).unwrap();
            let head = r#""repo":"demo","source":"s","created":1,"changed":2"#;
            std::fs::write(
                dir.join("1.json"),
                format!(
                    r#"{{{head},"number":1,"kind":"proposal","title":"P","state":"planned","plan":2,"body":"ask"}}"#
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join("2.json"),
                format!(
                    r##"{{{head},"number":2,"kind":"plan","title":"Old plan","state":"saved","proposals":[1],"body":"# Old plan\n"}}"##
                ),
            )
            .unwrap();
            std::fs::write(dir.join("last"), "2").unwrap();
            assert_eq!(migrate(&state), Ok(2));
            assert_eq!(migrate(&state), Ok(0));
            let proposal = load(&state, &config, "demo", 1).unwrap();
            assert_eq!((proposal.state, proposal.into), (Merged, Some(2)));
            let plan = load(&state, &config, "demo", 2).unwrap();
            assert_eq!(
                (plan.state, plan.plan_text.as_str(), plan.merged),
                (Review, "# Old plan\n", vec![1])
            );
        }

        fn onboarding_config() -> CiConfig {
            CiConfig::parse(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n\
                 [repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\n\
                 [secrets]\naddr = \"http://127.0.0.1:1\"\npath = \"zeughaus/ci\"\nrole_id = \"r\"\n\
                 [onboarding]\napi = \"https://git.example.org\"\nbot = \"ci\"\n\
                 bot_token = \"BOT\"\nadmin_token = \"ADMIN\"\nhook_url = \"https://ci.example.org/hook\"\n",
            )
            .expect("a valid ci.toml")
        }

        fn request<'a>(spec: &'a OnboardSpec) -> NewDoc<'a> {
            NewDoc {
                kind: DocKind::Onboarding,
                title: "join",
                body: "body",
                source: "scan-rs",
                links: &[],
                runs_on: &[],
                onboard: Some(spec),
            }
        }

        fn spec(url: &str) -> OnboardSpec {
            OnboardSpec {
                url: url.to_owned(),
                default_branch: "main".to_owned(),
                grants: Default::default(),
                prerequisites: Vec::new(),
            }
        }

        #[test]
        fn a_pending_repository_takes_tickets_and_shows_in_the_summary() {
            let state = scratch("pending");
            let config = onboarding_config();
            let asked = spec("git@git.example.org:O/Scan-RS.git");
            assert_eq!(
                create(&state, &config, "scan-rs", request(&asked), 1),
                Ok(1)
            );
            let stored = load(&state, &config, "scan-rs", 1).unwrap();
            assert_eq!(
                stored.onboard.unwrap().url,
                "https://git.example.org/o/scan-rs.git"
            );
            assert_eq!(stored.state, Open);
            assert_eq!(create(&state, &config, "scan-rs", new("p", &[]), 2), Ok(2));
            assert_eq!(
                create(&state, &config, "scan-rs", new("q", &depends(&[2])), 3),
                Err("scan-rs is not in ci.toml yet; links wait for its onboarding".to_owned())
            );
            assert_eq!(
                write_plan(&state, &config, "scan-rs", None, plan(&[]), 3),
                Err("scan-rs is not in ci.toml yet; plans wait for its onboarding".to_owned())
            );
            let summary = summary(&state, &config);
            let names: Vec<&str> = summary.iter().map(|r| r.repo.as_str()).collect();
            assert_eq!(names, ["demo", "scan-rs"]);
            let pending = &summary[1];
            assert_eq!(pending.onboarding, Some(1));
            assert_eq!((pending.awaiting, pending.open), (1, 1));
            assert_eq!(pending.origin, "git.example.org/o/scan-rs");
            assert_eq!(summary[0].onboarding, None);

            let again = create(
                &state,
                &config,
                "other",
                request(&spec("https://git.example.org/o/scan-rs")),
                4,
            );
            assert_eq!(
                again,
                Err("https://git.example.org/o/scan-rs is requested as scan-rs in #1".to_owned())
            );
            let twice = create(
                &state,
                &config,
                "scan-rs",
                request(&spec("https://git.example.org/o/x")),
                4,
            );
            assert_eq!(
                twice,
                Err("scan-rs has onboarding request #1 already (open)".to_owned())
            );
            let known = create(
                &state,
                &config,
                "x",
                request(&spec("https://git.example.org/o/demo.git")),
                4,
            );
            assert_eq!(
                known,
                Err("https://git.example.org/o/demo.git is repository demo already".to_owned())
            );
        }

        #[test]
        fn the_ci_finishes_an_approved_request_or_returns_it_with_a_note() {
            let state = scratch("finish");
            let config = onboarding_config();
            let asked = spec("https://git.example.org/o/scan-rs");
            create(&state, &config, "scan-rs", request(&asked), 1).unwrap();
            assert!(approved_onboardings(&state).is_empty());
            set_state(&state, &config, "scan-rs", 1, Approved, 2).unwrap();
            assert_eq!(approved_onboardings(&state).len(), 1);
            assert_eq!(summary(&state, &config)[1].in_progress, vec![1]);

            finish_onboarding(&state, "scan-rs", 1, Err("collaborator: 403".to_owned()), 3)
                .unwrap();
            let failed = load(&state, &config, "scan-rs", 1).unwrap();
            assert_eq!(
                (failed.state, failed.note.as_str()),
                (Open, "collaborator: 403")
            );
            assert!(finish_onboarding(&state, "scan-rs", 1, Ok(()), 4).is_err());

            set_state(&state, &config, "scan-rs", 1, Approved, 5).unwrap();
            finish_onboarding(&state, "scan-rs", 1, Ok(()), 6).unwrap();
            let done =
                read_json::<DocView>(&path_of(&docs_root(&state).join("scan-rs"), 1)).unwrap();
            assert_eq!((done.state, done.note.as_str()), (Done, ""));
            // Done but not in ci.toml: no longer pending, so hidden.
            assert!(list(&state, &config, "scan-rs").is_empty());
        }

        #[test]
        fn a_job_request_is_queued_once_while_its_ticket_is_open() {
            let state = scratch("job-request");
            let config = CiConfig::parse(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n\
                 [repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\nagent_sessions = true\n",
            )
            .expect("a valid ci.toml");
            let body = "# Fix the sync\n\nRuns on: os=linux\n\nerror: maybe\n";

            assert_eq!(
                file_request(&state, &config, "demo", "sync", body, 1),
                Ok(Filed::Queued(1))
            );
            let ticket = load(&state, &config, "demo", 1).expect("stored");
            assert_eq!(
                (
                    ticket.kind,
                    ticket.state,
                    ticket.title.as_str(),
                    ticket.source.as_str(),
                    ticket.plan_text.as_str(),
                ),
                (DocKind::Ticket, Approved, "Fix the sync", "job.sync", body)
            );
            assert_eq!(ticket.runs_on, vec!["os=linux".to_owned()]);
            assert_eq!(ticket.approved, Some(1));

            assert_eq!(
                file_request(&state, &config, "demo", "sync", body, 2),
                Ok(Filed::StillOpen(1))
            );
            assert_eq!(list(&state, &config, "demo").len(), 1);

            assert_eq!(
                file_request(&state, &config, "demo", "other", body, 3),
                Ok(Filed::Queued(2))
            );
            assert_eq!(
                file_request(&state, &config, "demo", "sync", "# Else\n", 4),
                Ok(Filed::Queued(3))
            );

            set_state(&state, &config, "demo", 1, Dismissed, 5).expect("dismiss");
            assert_eq!(
                file_request(&state, &config, "demo", "sync", body, 6),
                Ok(Filed::Queued(4))
            );

            let plain = scratch("job-request-refused");
            assert_eq!(
                file_request(&plain, &self::config(), "demo", "sync", body, 7),
                Err("demo does not allow agent sessions (agent_sessions in ci.toml)".to_owned())
            );
            assert!(!docs_root(&plain).exists());
            let _ = std::fs::remove_dir_all(&state);
            let _ = std::fs::remove_dir_all(&plain);
        }
    }
}
