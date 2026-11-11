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

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use zeughaus_link::ci::{
    DocKind, DocLink, DocState, DocView, LinkKind, MAX_COMMENT_BYTES, MAX_COMMENTS, MAX_DOC_BYTES,
    MAX_DOC_TITLE_BYTES, MAX_REPORT_BYTES, OnboardSpec, Planner, RepoDocsView, TicketComment,
    TicketReport, TicketSuggestion, normalize_origin, plan_title, report_text, runs_on_named,
};
use zeughaus_link::selector::parse_runs_on;
use zeughaus_link::{
    FiledTicket, NewTicket, TicketRef, compose_text, filing_order, plan_sections, section_text,
    ticket_text,
};

use crate::config::{CiConfig, valid_name};
use crate::files::{read_json, write_atomic, write_json};

/// The most links one ticket carries, and the most tickets one plan
/// includes.
const MAX_LINKS: usize = 32;

/// The most tickets one onboarding files.
const MAX_ONBOARD_TICKETS: usize = 24;

/// The longest `source` a document may carry.
const MAX_SOURCE_BYTES: usize = 100;

/// What [`create`] stores.
pub struct NewDoc<'a> {
    pub kind: DocKind,
    /// The Markdown text: its `# ` title line, a ticket's link lines, the
    /// rest.
    pub body: &'a str,
    pub source: &'a str,
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

/// Every document of `repo` without its proposal, plan, review, comments
/// and suggestion and with its report's text empty, in number order,
/// `waits_for` filled; nothing for a repository that is not in `ci.toml`.
pub fn list(state_dir: &Path, config: &CiConfig, repo: &str) -> Vec<DocView> {
    let Ok(dir) = dir(state_dir, config, repo) else {
        return Vec::new();
    };
    let mut docs = read_all(&dir);
    for doc in &mut docs {
        doc.body.clear();
        doc.plan_text.clear();
        doc.feedback.clear();
        doc.comments.clear();
        doc.suggestion = None;
        if let Some(report) = &mut doc.report {
            report.text.clear();
        }
    }
    fill_waits(state_dir, &mut docs);
    docs
}

/// Document `number` of `repo` with its body, `waits_for` filled.
pub fn load(state_dir: &Path, config: &CiConfig, repo: &str, number: u64) -> Option<DocView> {
    let dir = dir(state_dir, config, repo).ok()?;
    let mut doc: DocView = read_json(&path_of(&dir, number)).ok()?;
    fill_waits(state_dir, std::slice::from_mut(&mut doc));
    Some(doc)
}

/// The tickets of `repo` that are not finished, with their proposals but
/// without plan, review, comments, suggestion and report, in number order.
pub fn unfinished(state_dir: &Path, config: &CiConfig, repo: &str) -> Vec<DocView> {
    let Ok(dir) = dir(state_dir, config, repo) else {
        return Vec::new();
    };
    let mut docs = read_all(&dir);
    docs.retain(|doc| {
        doc.kind == DocKind::Ticket
            && matches!(
                doc.state,
                DocState::Open
                    | DocState::Ready
                    | DocState::Planning
                    | DocState::Review
                    | DocState::Approved
                    | DocState::InProgress
            )
    });
    for doc in &mut docs {
        doc.plan_text.clear();
        doc.feedback.clear();
        doc.comments.clear();
        doc.suggestion = None;
        doc.report = None;
    }
    docs
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

/// `links` of ticket `number` of `repo` without duplicates, a link into
/// `repo` itself written without its repository, or why they are refused:
/// too many, a link to itself, a target that is no ticket, or a
/// dependency that closes a cycle through the stored tickets of any
/// repository.
fn check_links(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    links: &[DocLink],
) -> Result<Vec<DocLink>, String> {
    let mut clean: Vec<DocLink> = Vec::with_capacity(links.len());
    for link in links {
        let mut link = link.clone();
        if link.repo.as_deref() == Some(repo) {
            link.repo = None;
        }
        if !clean.contains(&link) {
            clean.push(link);
        }
    }
    if clean.len() > MAX_LINKS {
        return Err(format!("a ticket has at most {MAX_LINKS} links"));
    }
    // Each repository's tickets, read once.
    let cache: RefCell<BTreeMap<String, Vec<DocView>>> = RefCell::new(BTreeMap::new());
    let stored = |of: &str, n: u64| -> Option<(DocKind, Vec<DocLink>)> {
        if !valid_name(of) {
            return None;
        }
        let mut cache = cache.borrow_mut();
        let docs = cache
            .entry(of.to_owned())
            .or_insert_with(|| read_all(&docs_root(state_dir).join(of)));
        docs.iter()
            .find(|doc| doc.number == n)
            .map(|doc| (doc.kind, doc.links.clone()))
    };
    for link in &clean {
        let target = link.repo.as_deref().unwrap_or(repo);
        if link.repo.is_some() {
            dir(state_dir, config, target)?;
        } else if link.number == number {
            return Err(format!("#{number} cannot link to itself"));
        }
        match stored(target, link.number) {
            None => return Err(format!("no ticket #{} in {target}", link.number)),
            Some((kind, _)) if kind != DocKind::Ticket => {
                return Err(format!("{} is no ticket", link.reference(repo)));
            }
            Some(_) => {}
        }
    }
    let own = (repo.to_owned(), number);
    let absolute = |of: &str, links: &[DocLink]| -> Vec<(String, u64)> {
        links
            .iter()
            .filter(|link| link.kind == LinkKind::DependsOn)
            .map(|link| (link.repo.as_deref().unwrap_or(of).to_owned(), link.number))
            .collect()
    };
    // Who each ticket depends on, with the new set in place of `number`'s.
    let depends = |node: &(String, u64)| -> Vec<(String, u64)> {
        if *node == own {
            return absolute(repo, &clean);
        }
        stored(&node.0, node.1)
            .map(|(_, links)| absolute(&node.0, &links))
            .unwrap_or_default()
    };
    let named = |node: &(String, u64)| {
        if node.0 == repo {
            format!("#{}", node.1)
        } else {
            format!("#{}-{}", node.0, node.1)
        }
    };
    for link in clean.iter().filter(|link| link.kind == LinkKind::DependsOn) {
        let target = (link.repo.as_deref().unwrap_or(repo).to_owned(), link.number);
        if let Some(path) = path_to(target, &own, &depends) {
            let through = &path[1..path.len() - 1];
            if through.is_empty() {
                return Err(format!(
                    "#{number} depends on {}, which depends on #{number}",
                    link.reference(repo)
                ));
            }
            let names: Vec<String> = through.iter().map(named).collect();
            return Err(format!(
                "a dependency on {} would close a cycle through {}",
                link.reference(repo),
                names.join(", ")
            ));
        }
    }
    Ok(clean)
}

/// A dependency path from `from` to `to`, both included, by a depth-first
/// walk over `depends`; `None` when `to` is not reached.
fn path_to<N: Clone + Ord>(from: N, to: &N, depends: &dyn Fn(&N) -> Vec<N>) -> Option<Vec<N>> {
    fn walk<N: Clone + Ord>(
        at: &N,
        to: &N,
        depends: &dyn Fn(&N) -> Vec<N>,
        seen: &mut BTreeSet<N>,
        path: &mut Vec<N>,
    ) -> bool {
        if at == to {
            return true;
        }
        if !seen.insert(at.clone()) {
            return false;
        }
        for next in depends(at) {
            path.push(next.clone());
            if walk(&next, to, depends, seen, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut seen = BTreeSet::new();
    let mut path = vec![from.clone()];
    walk(&from, to, depends, &mut seen, &mut path).then_some(path)
}

/// What a waiting ticket needs to know of a target: its state and the
/// ticket it is merged into.
type Found = (DocState, Option<u64>);

/// The `depends on` targets of `doc` that are not finished, in link order.
/// A target's repository is the link's, else `doc`'s; `find` answers what
/// it knows of a ticket. Finished is `done` or `dismissed`, a ticket `find`
/// does not know (deleted), or `merged` into a ticket that is itself
/// finished; a target merged into `doc` itself is its own work and blocks
/// nothing.
fn waits_for(doc: &DocView, find: &mut dyn FnMut(&str, u64) -> Option<Found>) -> Vec<DocLink> {
    let finished = |state: DocState| matches!(state, DocState::Done | DocState::Dismissed);
    let mut waiting = Vec::new();
    for link in doc
        .links
        .iter()
        .filter(|link| link.kind == LinkKind::DependsOn)
    {
        let repo = link.repo.as_deref().unwrap_or(&doc.repo);
        let Some((state, into)) = find(repo, link.number) else {
            continue;
        };
        let open = match (state, into) {
            (DocState::Merged, None) => false,
            (DocState::Merged, Some(into)) => {
                !(repo == doc.repo && into == doc.number)
                    && find(repo, into).is_some_and(|(state, _)| !finished(state))
            }
            (state, _) => !finished(state),
        };
        if open {
            waiting.push(link.clone());
        }
    }
    waiting
}

/// Fills `waits_for` of every document in `docs`, looking targets up in
/// `docs` first and then in their repositories' stored tickets.
fn fill_waits(state_dir: &Path, docs: &mut [DocView]) {
    let known: BTreeMap<(String, u64), Found> = docs
        .iter()
        .map(|doc| ((doc.repo.clone(), doc.number), (doc.state, doc.into)))
        .collect();
    let mut others: BTreeMap<String, Vec<DocView>> = BTreeMap::new();
    for doc in docs.iter_mut() {
        let mut find = |repo: &str, number: u64| {
            if let Some(&found) = known.get(&(repo.to_owned(), number)) {
                return Some(found);
            }
            if !valid_name(repo) {
                return None;
            }
            others
                .entry(repo.to_owned())
                .or_insert_with(|| read_all(&docs_root(state_dir).join(repo)))
                .iter()
                .find(|other| other.number == number)
                .map(|other| (other.state, other.into))
        };
        let waiting = waits_for(doc, &mut find);
        doc.waits_for = waiting;
    }
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
    let text = ticket_text(new.body)?;
    check_body(new.body)?;
    if !valid_source(new.source) {
        return Err(format!(
            "a source is 1 to {MAX_SOURCE_BYTES} of A-Z a-z 0-9 . _ -"
        ));
    }
    parse_runs_on(new.runs_on)?;
    if !text.links.is_empty() {
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
    let links = check_links(state_dir, config, repo, number, &text.links)?;
    let doc = DocView {
        repo: repo.to_owned(),
        number,
        kind: new.kind,
        title: text.title,
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
        auto_approve: false,
        feedback: String::new(),
        onboard,
        note: String::new(),
        waits_for: Vec::new(),
        filed: Vec::new(),
        comments: Vec::new(),
        suggestion: None,
        report: None,
    };
    write_json(&path_of(&dir, number), &doc)?;
    write_atomic(&dir.join("last"), number.to_string().as_bytes())?;
    Ok(number)
}

/// Onboards `repo` in one step for its owner: stores its onboarding
/// request already `approved` (the onboarding worker takes it from there)
/// and `tickets` behind it, `ready` and in their order, each linked to the
/// tickets it depends on by their index and stored as Markdown with those
/// links. The tickets wait in the planning queue until the repository has
/// joined `ci.toml`. Answers the request's number and the tickets'. A
/// refusal writes nothing.
pub fn onboard(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    request: NewDoc<'_>,
    tickets: &[NewTicket],
    now: u64,
) -> Result<(u64, Vec<u64>), String> {
    let Some(spec) = request.onboard else {
        return Err("an onboarding request names the repository it asks for".to_owned());
    };
    let spec = crate::onboard::check_request(state_dir, config, repo, spec)?;
    let text = ticket_text(request.body)?;
    if !text.links.is_empty() {
        return Err("only a ticket has links".to_owned());
    }
    check_body(request.body)?;
    if !valid_source(request.source) {
        return Err(format!(
            "a source is 1 to {MAX_SOURCE_BYTES} of A-Z a-z 0-9 . _ -"
        ));
    }
    if tickets.len() > MAX_ONBOARD_TICKETS {
        return Err(format!(
            "an onboarding files at most {MAX_ONBOARD_TICKETS} tickets"
        ));
    }
    for (at, ticket) in tickets.iter().enumerate() {
        checked_title(&ticket.title)?;
        check_body(&ticket.body)?;
        if ticket.depends_on.len() > MAX_LINKS {
            return Err(format!("a ticket has at most {MAX_LINKS} links"));
        }
        if let Some(bad) = ticket
            .depends_on
            .iter()
            .find(|&&on| on >= tickets.len() || on == at)
        {
            return Err(format!(
                "ticket {} depends on {bad}, which is no other ticket of the onboarding",
                at + 1
            ));
        }
    }
    let depends = |of: &u64| -> Vec<u64> {
        tickets
            .get(*of as usize)
            .map(|t| t.depends_on.iter().map(|&on| on as u64).collect())
            .unwrap_or_default()
    };
    for (at, ticket) in tickets.iter().enumerate() {
        for &on in &ticket.depends_on {
            if path_to(on as u64, &(at as u64), &depends).is_some() {
                return Err(format!(
                    "the onboarding's tickets {} and {} depend on each other",
                    at + 1,
                    on + 1
                ));
            }
        }
    }
    let dir = docs_root(state_dir).join(repo);
    let first = last_number(&dir) + 1;
    let numbers: Vec<u64> = (0..tickets.len() as u64).map(|at| first + 1 + at).collect();
    let doc = |number: u64, kind: DocKind, state: DocState, title: &str, body: &str| DocView {
        repo: repo.to_owned(),
        number,
        kind,
        title: title.to_owned(),
        state,
        source: request.source.to_owned(),
        created: now,
        changed: now,
        approved: None,
        released: None,
        links: Vec::new(),
        merged: Vec::new(),
        into: None,
        runs_on: Vec::new(),
        placement: Vec::new(),
        body: body.to_owned(),
        plan_text: String::new(),
        revision: 0,
        planner: None,
        auto_approve: false,
        feedback: String::new(),
        onboard: None,
        note: String::new(),
        waits_for: Vec::new(),
        filed: Vec::new(),
        comments: Vec::new(),
        suggestion: None,
        report: None,
    };
    let mut written = vec![DocView {
        approved: Some(now),
        onboard: Some(spec),
        ..doc(
            first,
            DocKind::Onboarding,
            DocState::Approved,
            &text.title,
            request.body,
        )
    }];
    for (ticket, &number) in tickets.iter().zip(&numbers) {
        let mut links: Vec<DocLink> = Vec::new();
        for &on in &ticket.depends_on {
            let link = DocLink {
                kind: LinkKind::DependsOn,
                number: numbers[on],
                repo: None,
            };
            if !links.contains(&link) {
                links.push(link);
            }
        }
        let title = ticket.title.trim();
        let body = compose_text(title, &links, &ticket.body);
        written.push(DocView {
            released: Some(now),
            links,
            auto_approve: ticket.auto_approve,
            ..doc(number, DocKind::Ticket, DocState::Ready, title, &body)
        });
    }
    // The tickets first, the request last: the onboarding worker acts on
    // an approved request, and its tickets are to be there when it does.
    for written in written.iter().skip(1).chain(written.first()) {
        write_json(&path_of(&dir, written.number), written)?;
    }
    let last = numbers.last().copied().unwrap_or(first);
    write_atomic(&dir.join("last"), last.to_string().as_bytes())?;
    Ok((first, numbers))
}

/// The ticket at `path`, or that it is none.
fn read_ticket(path: &Path, repo: &str, number: u64) -> Result<DocView, String> {
    let doc: DocView = read_json(path).map_err(|_| missing(repo, number))?;
    if doc.kind != DocKind::Ticket {
        return Err(format!("#{number} is no ticket"));
    }
    Ok(doc)
}

/// Replaces the text of a ticket that is not finished; its title and links
/// are read from it again.
pub fn edit(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    body: &str,
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
            "#{number} is {}; its text is fixed",
            doc.state.name()
        ));
    }
    let text = ticket_text(body)?;
    check_body(body)?;
    if !text.links.is_empty() && !config.repos.contains_key(repo) {
        return Err(format!(
            "{repo} is not in ci.toml yet; links wait for its onboarding"
        ));
    }
    doc.links = check_links(state_dir, config, repo, number, &text.links)?;
    doc.title = text.title;
    doc.body = body.to_owned();
    doc.suggestion = None;
    doc.changed = now;
    write_json(&path, &doc)
}

/// Adds a comment by `author` to a ticket in any state.
pub fn comment(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    author: &str,
    text: &str,
    now: u64,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    let author = author.trim();
    if author.is_empty() || author.len() > MAX_SOURCE_BYTES || author.contains(['\n', '\r']) {
        return Err(format!(
            "an author is one line of 1 to {MAX_SOURCE_BYTES} bytes"
        ));
    }
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_COMMENT_BYTES {
        return Err(format!("a comment is 1 to {MAX_COMMENT_BYTES} bytes"));
    }
    if doc.comments.len() >= MAX_COMMENTS {
        return Err(format!(
            "#{number} has {MAX_COMMENTS} comments; it takes no more"
        ));
    }
    doc.comments.push(TicketComment {
        author: author.to_owned(),
        at: now,
        text: text.to_owned(),
    });
    // The editor reloads a shown ticket when `changed` moves.
    doc.changed = now;
    write_json(&path, &doc)
}

/// Keeps the advice model's answer on a ticket that may still be
/// re-evaluated; one that moved on while the model thought drops it.
pub fn set_suggestion(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    suggestion: TicketSuggestion,
    now: u64,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if !doc.state.reevaluable() {
        return Err(format!(
            "#{number} is {} now; the suggestion is dropped",
            doc.state.name()
        ));
    }
    doc.suggestion = Some(suggestion);
    doc.changed = now;
    write_json(&path, &doc)
}

/// Pre-approves the plan of a ticket that is open, ready or planning, or
/// takes that back: a pre-approved plan is approved as it arrives.
pub fn set_auto_approve(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    on: bool,
    now: u64,
) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if !matches!(
        doc.state,
        DocState::Open | DocState::Ready | DocState::Planning
    ) {
        return Err(format!(
            "#{number} is {}; pre-approval is set before its plan is written",
            doc.state.name()
        ));
    }
    doc.auto_approve = on;
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

/// Stores a ticket that runs through, already approved: `body` is its text
/// (title and links read as [`create`] reads them), `plan` its plan, whose
/// `Runs on` line places it. A refusal before the ticket is stored writes
/// nothing.
pub fn file_approved(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    body: &str,
    plan: &str,
    source: &str,
    now: u64,
) -> Result<u64, String> {
    if plan.is_empty() || plan.len() > MAX_DOC_BYTES {
        return Err(format!("a plan is 1 to {MAX_DOC_BYTES} bytes"));
    }
    let runs_on = runs_on_named(plan)?;
    let new = NewDoc {
        kind: DocKind::Ticket,
        body,
        source,
        runs_on: &runs_on,
        onboard: None,
    };
    let number = create(state_dir, config, repo, new, now)?;
    let path = path_of(&dir(state_dir, config, repo)?, number);
    let mut doc = read_ticket(&path, repo, number)?;
    doc.plan_text = plan.to_owned();
    doc.revision = 1;
    doc.auto_approve = true;
    doc.state = DocState::Approved;
    doc.approved = Some(now);
    write_json(&path, &doc)?;
    Ok(number)
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
/// ticket keeps its stamp and with it its place. Approving a plan files
/// its sections for other tickets first; one that cannot be filed keeps the
/// ticket in review. The tickets a plan includes follow it: `done` with
/// it, back to `open` when it is dismissed or its plan discarded.
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
            if doc.kind == DocKind::Ticket {
                file_sections(state_dir, config, repo, &mut doc, &path, now)
                    .map_err(|e| format!("its sections were not filed: {e}"))?;
            }
            doc.approved = Some(now);
            doc.feedback.clear();
        }
        (DocState::Review, DocState::Open) => {
            doc.plan_text.clear();
            doc.planner = None;
            doc.feedback.clear();
            doc.report = None;
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
    doc.suggestion = None;
    if to == DocState::Done {
        doc.note.clear();
    }
    doc.changed = now;
    write_json(&path, &doc)?;
    if reopen || close {
        settle_merged(&dir, number, &merged, close, now)?;
    }
    Ok(())
}

/// Marks in-progress ticket `number` done, as [`set_state`] does, and keeps
/// `report`, the executing session's final report, on it.
pub fn finish(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    report: &str,
    now: u64,
) -> Result<(), String> {
    let text = report.trim();
    if text.is_empty() || text.len() > MAX_REPORT_BYTES {
        return Err(format!("a report is 1 to {MAX_REPORT_BYTES} bytes"));
    }
    set_state(state_dir, config, repo, number, DocState::Done, now)?;
    let path = path_of(&dir(state_dir, config, repo)?, number);
    let mut doc = read_ticket(&path, repo, number)?;
    doc.report = Some(TicketReport {
        text: text.to_owned(),
        written: now,
        from_transcript: false,
    });
    write_json(&path, &doc)
}

/// Stores a plan on ticket `number`, which a plan session works on, or as a
/// new ticket without one, and answers the ticket's number; either is then
/// in `review`. The tickets the plan includes become `merged` into it, and
/// those an earlier revision included and this one does not go back to
/// `open`. Its sections for other tickets must resolve. A refusal writes
/// nothing.
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
    check_sections(state_dir, config, repo, plan.plan)?;
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
                auto_approve: false,
                feedback: String::new(),
                onboard: None,
                note: String::new(),
                waits_for: Vec::new(),
                filed: Vec::new(),
                comments: Vec::new(),
                suggestion: None,
                report: None,
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
    doc.suggestion = None;
    doc.report = None;
    doc.note.clear();
    if !plan.runs_on.is_empty() {
        doc.runs_on = plan.runs_on.to_vec();
    }
    doc.merged = includes;
    // A ticket that runs through needs no review: its plan is approved as
    // it arrives, and its master is queued.
    if doc.auto_approve {
        doc.state = DocState::Approved;
        doc.approved = Some(now);
    } else {
        doc.state = DocState::Review;
    }
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
    // A ticket approved as its plan arrives files its sections now; should
    // that fail, it waits in review with the reason.
    if doc.state == DocState::Approved {
        let path = path_of(&dir, number);
        if let Err(e) = file_sections(state_dir, config, repo, &mut doc, &path, now) {
            doc.state = DocState::Review;
            doc.approved = None;
            doc.note = format!("its sections were not filed: {e}");
            write_json(&path, &doc)?;
        }
    }
    Ok(number)
}

/// Whether `name` is a repository a plan's section may be filed in: one of
/// `ci.toml`, or one with a pending onboarding request.
fn known_repo(state_dir: &Path, config: &CiConfig, name: &str) -> bool {
    config.repos.contains_key(name) || pending_request(state_dir, config, name).is_some()
}

/// Why `plan` of `repo` cannot be stored: a section whose references do
/// not resolve to stored tickets, or with link lines for a repository that
/// has only an onboarding request.
fn check_sections(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    plan: &str,
) -> Result<(), String> {
    let sections = plan_sections(plan, |name| known_repo(state_dir, config, name))?;
    for section in &sections {
        let refs: Vec<&TicketRef> = section.depends.iter().chain(&section.related).collect();
        if !refs.is_empty() && !config.repos.contains_key(&section.repo) {
            return Err(format!(
                "{} is not in ci.toml yet; its section \"{}\" cannot carry Depends on or Related lines",
                section.repo, section.title
            ));
        }
        for link in refs {
            let (target, number) = match link {
                TicketRef::Same(number) => (repo, *number),
                TicketRef::Other { repo, number } => (repo.as_str(), *number),
                TicketRef::SelfRef | TicketRef::Label(_) => continue,
            };
            let dir = dir(state_dir, config, target)?;
            match read_json::<DocView>(&path_of(&dir, number)) {
                Ok(doc) if doc.kind == DocKind::Ticket => {}
                _ => return Err(format!("no ticket #{number} in {target}")),
            }
        }
    }
    Ok(())
}

/// Files each section of `doc`'s plan for another ticket that is not filed
/// yet as a `ready` ticket of its repository, in [`filing_order`], and
/// records it in `doc.filed`, written to `path` after each, so a failure
/// keeps what was filed and a later approval files only the rest.
fn file_sections(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    doc: &mut DocView,
    path: &Path,
    now: u64,
) -> Result<(), String> {
    let sections = plan_sections(&doc.plan_text, |name| known_repo(state_dir, config, name))?;
    let filed_as = |filed: &[FiledTicket], repo: &str, title: &str| {
        filed
            .iter()
            .find(|ticket| ticket.repo == repo && ticket.title == title)
            .map(|ticket| ticket.number)
    };
    let mut labels: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for section in &sections {
        if let (Some(label), Some(number)) = (
            &section.label,
            filed_as(&doc.filed, &section.repo, &section.title),
        ) {
            labels.insert(label.clone(), (section.repo.clone(), number));
        }
    }
    for at in filing_order(&sections) {
        let section = &sections[at];
        if filed_as(&doc.filed, &section.repo, &section.title).is_some() {
            continue;
        }
        let links_allowed = config.repos.contains_key(&section.repo);
        let body = section_text(section, repo, doc.number, &labels, links_allowed)?;
        let new = NewDoc {
            kind: DocKind::Ticket,
            body: &body,
            source: repo,
            runs_on: &[],
            onboard: None,
        };
        let number = create(state_dir, config, &section.repo, new, now)?;
        set_state(
            state_dir,
            config,
            &section.repo,
            number,
            DocState::Ready,
            now,
        )?;
        if let Some(label) = &section.label {
            labels.insert(label.clone(), (section.repo.clone(), number));
        }
        doc.filed.push(FiledTicket {
            repo: section.repo.clone(),
            number,
            title: section.title.clone(),
        });
        write_json(path, doc)?;
    }
    Ok(())
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
    doc.note.clear();
    doc.changed = now;
    write_json(&path, &doc)?;
    Ok(doc)
}

/// What became of the master of an in-progress ticket.
pub enum MasterEnd<'a> {
    /// It never ran: the runner refused it with this message.
    Refused(&'a str),
    /// Its process was killed or went away with its runner.
    Lost,
    /// It ended by itself or was stopped; `why` says how.
    Stopped(&'a str),
}

/// The master of ticket `number` ended. Answers whether the ticket changed;
/// a ticket that is not in progress any more is left alone. A refused
/// master puts the ticket back into the queue with the reason and without
/// its placement; a lost one puts it back with its placement, so its runner
/// takes it again and resumes; a stopped one leaves it in progress with a
/// note and keeps `report`, its last words, on it. `approved` is kept, so a
/// requeued ticket keeps its place.
pub fn master_ended(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    end: MasterEnd<'_>,
    report: Option<&str>,
    now: u64,
) -> Result<bool, String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let mut doc = read_ticket(&path, repo, number)?;
    if doc.state != DocState::InProgress {
        return Ok(false);
    }
    match end {
        MasterEnd::Refused(message) => {
            doc.state = DocState::Approved;
            doc.placement.clear();
            doc.note = format!("the master could not start: {message}");
        }
        MasterEnd::Lost => {
            doc.state = DocState::Approved;
            doc.note.clear();
        }
        MasterEnd::Stopped(why) => {
            doc.note =
                format!("the master ended ({why}) with the ticket in progress; Requeue resumes it");
            if let Some(text) = report.map(report_text).filter(|text| !text.is_empty()) {
                doc.report = Some(TicketReport {
                    text: text.to_owned(),
                    written: now,
                    from_transcript: true,
                });
            }
        }
    }
    doc.changed = now;
    write_json(&path, &doc)?;
    Ok(true)
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
/// ticket, and every text from before Markdown tickets with its title and
/// links in it, and answers how many documents it changed. A file that
/// does not parse is left alone.
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
            let ticket = migrate_doc(&mut doc);
            let mut view = serde_json::from_value::<DocView>(doc.clone()).ok();
            if let Some(view) = view.take_if(markdown_migrated) {
                write_json(&path, &view)?;
            } else if ticket {
                write_json(&path, &doc)?;
            } else {
                continue;
            }
            changed += 1;
        }
    }
    Ok(changed)
}

/// A ticket's or onboarding request's text from before Markdown tickets,
/// whose first line is not its `# ` title, as its title line, a ticket's
/// link lines and the old text. Answers whether it changed anything.
fn markdown_migrated(doc: &mut DocView) -> bool {
    let titled = doc
        .body
        .lines()
        .find(|line| !line.trim().is_empty())
        .is_none_or(|line| line.starts_with("# "));
    if titled {
        return false;
    }
    let links: &[DocLink] = match doc.kind {
        DocKind::Ticket => &doc.links,
        DocKind::Onboarding => &[],
    };
    doc.body = compose_text(&doc.title, links, &doc.body);
    true
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

    fn ticket(
        repo: &str,
        number: u64,
        state: DocState,
        depends: &[(Option<&str>, u64)],
    ) -> DocView {
        DocView {
            repo: repo.to_owned(),
            number,
            kind: DocKind::Ticket,
            title: "T".to_owned(),
            state,
            source: "s".to_owned(),
            created: 1,
            changed: 1,
            approved: None,
            released: None,
            links: depends
                .iter()
                .map(|&(repo, number)| DocLink {
                    kind: LinkKind::DependsOn,
                    number,
                    repo: repo.map(str::to_owned),
                })
                .collect(),
            merged: Vec::new(),
            into: None,
            runs_on: Vec::new(),
            placement: Vec::new(),
            body: String::new(),
            plan_text: String::new(),
            revision: 0,
            planner: None,
            auto_approve: false,
            feedback: String::new(),
            onboard: None,
            note: String::new(),
            waits_for: Vec::new(),
            filed: Vec::new(),
            comments: Vec::new(),
            suggestion: None,
            report: None,
        }
    }

    #[test]
    fn only_unfinished_dependencies_hold_a_ticket_back_across_repositories() {
        let mut docs = vec![
            ticket("demo", 1, Done, &[]),
            ticket("demo", 2, Dismissed, &[]),
            ticket("demo", 3, Open, &[]),
            ticket("demo", 4, Merged, &[]),
            ticket("demo", 5, Review, &[]),
            ticket("weida", 3, Done, &[]),
            ticket("weida", 7, Ready, &[]),
        ];
        docs[3].into = Some(5);
        // Depends on the done, the dismissed, a deleted (#9), the open and
        // the merged ticket, weida's done #3 and weida's ready #7.
        let mut waiting = ticket(
            "demo",
            6,
            Ready,
            &[
                (None, 1),
                (None, 2),
                (None, 9),
                (None, 3),
                (None, 4),
                (Some("weida"), 3),
                (Some("weida"), 7),
            ],
        );
        waiting.links.push(DocLink {
            kind: LinkKind::Related,
            number: 5,
            repo: None,
        });
        let numbers = |docs: &[DocView], waiting: &DocView| -> Vec<(Option<String>, u64)> {
            let mut find = |repo: &str, number: u64| {
                docs.iter()
                    .find(|d| d.repo == repo && d.number == number)
                    .map(|d| (d.state, d.into))
            };
            waits_for(waiting, &mut find)
                .into_iter()
                .map(|link| (link.repo, link.number))
                .collect()
        };
        assert_eq!(
            numbers(&docs, &waiting),
            vec![(None, 3), (None, 4), (Some("weida".to_owned()), 7)]
        );
        // The merged ticket follows the one that includes it.
        docs[4].state = Done;
        docs[6].state = Done;
        assert_eq!(numbers(&docs, &waiting), vec![(None, 3)]);
        docs[2].state = Dismissed;
        assert!(numbers(&docs, &waiting).is_empty());
        // A ticket merged into the waiting one is its own work.
        docs[3].into = Some(6);
        docs[4].state = Open;
        assert!(numbers(&docs, &waiting).is_empty());
    }

    #[test]
    fn a_text_from_before_markdown_tickets_gets_its_title_and_links() {
        let mut old = ticket("demo", 4, Open, &[(None, 2), (Some("weida"), 3)]);
        old.title = "Keep it".to_owned();
        old.body = "Please keep it.\n".to_owned();
        assert!(markdown_migrated(&mut old));
        assert_eq!(
            old.body,
            "# Keep it\nDepends on: #2, #weida-3\n\nPlease keep it.\n"
        );
        assert!(!markdown_migrated(&mut old));
        // A ticket that started as a plan has no text to migrate.
        let mut planned = ticket("demo", 5, Review, &[]);
        assert!(!markdown_migrated(&mut planned));
        let mut request = ticket("demo", 6, Open, &[(None, 2)]);
        request.kind = DocKind::Onboarding;
        request.body = "Join.".to_owned();
        assert!(markdown_migrated(&mut request));
        assert_eq!(request.body, "# T\n\nJoin.\n");
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
                 [repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\n\
                 [repos.weida]\nurl = \"https://git.example.org/o/weida.git\"\n",
            )
            .expect("a valid ci.toml")
        }

        /// The text of a ticket titled `title` that depends on tickets
        /// `depends` of its own repository.
        fn text(title: &str, depends: &[u64]) -> String {
            let mut text = format!("# {title}\n");
            if !depends.is_empty() {
                let refs: Vec<String> = depends.iter().map(|n| format!("#{n}")).collect();
                text.push_str(&format!("Depends on: {}\n", refs.join(", ")));
            }
            text.push_str("\nbody\n");
            text
        }

        fn new(body: &str) -> NewDoc<'_> {
            NewDoc {
                kind: DocKind::Ticket,
                body,
                source: "weida",
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
                    repo: None,
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
            let a = create(&state, &config, "demo", new(&text("a", &[])), 1);
            let b = write_plan(&state, &config, "demo", None, plan(&[]), 2);
            assert_eq!((a, b), (Ok(1), Ok(2)));
            set_state(&state, &config, "demo", 2, Dismissed, 3).expect("dismiss");
            delete(&state, &config, "demo", 2).expect("delete");
            let c = create(&state, &config, "demo", new(&text("c", &[])), 4);
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
                "# c\n\nbody\n"
            );
        }

        #[test]
        fn a_comment_is_kept_on_a_dismissed_ticket_and_left_out_of_lists() {
            let state = scratch("comment");
            let config = config();
            create(&state, &config, "demo", new(&text("a", &[])), 1).unwrap();
            set_state(&state, &config, "demo", 1, Dismissed, 2).unwrap();
            let said = comment(
                &state,
                &config,
                "demo",
                1,
                " hannes@tuco-pc ",
                "  still relevant?  ",
                3,
            );
            assert_eq!(said, Ok(()));
            let doc = load(&state, &config, "demo", 1).expect("stored");
            assert_eq!(
                doc.comments,
                vec![TicketComment {
                    author: "hannes@tuco-pc".to_owned(),
                    at: 3,
                    text: "still relevant?".to_owned(),
                }]
            );
            assert_eq!(doc.changed, 3);
            assert!(list(&state, &config, "demo")[0].comments.is_empty());
            assert_eq!(
                comment(&state, &config, "demo", 1, "hannes@tuco-pc", "  ", 4),
                Err("a comment is 1 to 8192 bytes".to_owned())
            );
            for at in 4..103 {
                comment(&state, &config, "demo", 1, "hannes@tuco-pc", "more", at).unwrap();
            }
            assert_eq!(
                comment(
                    &state,
                    &config,
                    "demo",
                    1,
                    "hannes@tuco-pc",
                    "one more",
                    103
                ),
                Err("#1 has 100 comments; it takes no more".to_owned())
            );
        }

        #[test]
        fn a_suggestion_is_kept_until_the_ticket_moves() {
            let state = scratch("suggestion");
            let config = config();
            create(&state, &config, "demo", new(&text("a", &[])), 1).unwrap();
            let suggestion = TicketSuggestion {
                verdict: zeughaus_link::Verdict::Dismiss,
                into: None,
                reason: "Done by abc1234.".to_owned(),
                text: String::new(),
                model: "m".to_owned(),
                created: 2,
            };
            set_suggestion(&state, &config, "demo", 1, suggestion.clone(), 2).unwrap();
            assert_eq!(
                load(&state, &config, "demo", 1).expect("stored").suggestion,
                Some(suggestion.clone())
            );
            set_state(&state, &config, "demo", 1, Ready, 3).unwrap();
            assert_eq!(
                load(&state, &config, "demo", 1).expect("stored").suggestion,
                None
            );
            set_state(&state, &config, "demo", 1, Open, 4).unwrap();
            set_state(&state, &config, "demo", 1, Dismissed, 5).unwrap();
            assert_eq!(
                set_suggestion(&state, &config, "demo", 1, suggestion, 6),
                Err("#1 is dismissed now; the suggestion is dropped".to_owned())
            );
        }

        #[test]
        fn a_plan_merges_the_tickets_it_includes_and_a_revision_lets_one_go() {
            let state = scratch("includes");
            let config = config();
            for title in ["main", "a", "b", "c"] {
                create(&state, &config, "demo", new(&text(title, &[])), 1).unwrap();
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
                create(&state, &config, "demo", new(&text(title, &[])), 1).unwrap();
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
            create(&state, &config, "demo", new(&text("a", &[])), 1).unwrap();
            create(&state, &config, "demo", new(&text("b", &[1])), 1).unwrap();
            create(&state, &config, "demo", new(&text("c", &[2])), 1).unwrap();
            let edit_to = |body: &str| edit(&state, &config, "demo", 1, body, 2);
            assert_eq!(
                edit_to(&text("a", &[2])),
                Err("#1 depends on #2, which depends on #1".to_owned())
            );
            assert_eq!(
                edit_to(&text("a", &[3])),
                Err("a dependency on #3 would close a cycle through #2".to_owned())
            );
            assert_eq!(
                edit_to(&text("a", &[1])),
                Err("#1 cannot link to itself".to_owned())
            );
            assert_eq!(
                edit_to(&text("a", &[9])),
                Err("no ticket #9 in demo".to_owned())
            );
            // A related link closes no cycle, and duplicates collapse.
            edit_to("# a2\nRelated: #3, #3, #demo-3\n").unwrap();
            let edited = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(edited.title, "a2");
            assert_eq!(
                edited.links,
                vec![DocLink {
                    kind: LinkKind::Related,
                    number: 3,
                    repo: None,
                }]
            );
        }

        #[test]
        fn a_ticket_reads_its_title_and_links_from_its_text_across_repositories() {
            let state = scratch("markdown");
            let config = config();
            create(&state, &config, "weida", new(&text("w", &[])), 1).unwrap();
            create(
                &state,
                &config,
                "demo",
                new("# Follow weida \nDepends on: #weida-1\n\nbody\n"),
                2,
            )
            .unwrap();
            let made = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(made.title, "Follow weida");
            assert_eq!(
                made.links,
                vec![DocLink {
                    kind: LinkKind::DependsOn,
                    number: 1,
                    repo: Some("weida".to_owned()),
                }]
            );
            assert_eq!(made.waits_for, made.links);
            assert_eq!(
                create(&state, &config, "demo", new("body\n"), 3),
                Err("a ticket's text starts with its \"# <title>\" line".to_owned())
            );
            assert_eq!(
                create(
                    &state,
                    &config,
                    "demo",
                    new("# x\nDepends on: #weida-9\n"),
                    3
                ),
                Err("no ticket #9 in weida".to_owned())
            );
            assert_eq!(
                create(
                    &state,
                    &config,
                    "demo",
                    new("# x\nDepends on: #other-1\n"),
                    3
                ),
                Err("no repository other".to_owned())
            );
            // weida #1 waiting for demo #1 closes a cycle across both.
            assert_eq!(
                edit(&state, &config, "weida", 1, "# w\nDepends on: #demo-1\n", 4),
                Err("#1 depends on #demo-1, which depends on #1".to_owned())
            );
            // Done, it holds nothing back, in lists too.
            assert_eq!(list(&state, &config, "demo")[0].waits_for.len(), 1);
            set_state(&state, &config, "weida", 1, Dismissed, 5).unwrap();
            assert!(list(&state, &config, "demo")[0].waits_for.is_empty());
            assert_eq!(
                edit(&state, &config, "weida", 1, "# w\n", 6),
                Err("#1 is dismissed; its text is fixed".to_owned())
            );
        }

        const SECTIONS: &str = "# Job state\n\
            \n\
            ## weida - Keep the fuzz corpus #fuzz\n\
            Depends on: #self\n\
            \n\
            fuzz.sh restores the corpus.\n\
            \n\
            ## demo - Follow up\n\
            Depends on: #fuzz\n\
            \n\
            Then this.\n";

        fn sections_plan(text: &str) -> PlanText<'_> {
            PlanText {
                title: "Job state",
                plan: text,
                includes: &[],
                runs_on: &[],
                source: "demo",
                planner: None,
            }
        }

        #[test]
        fn a_plan_with_sections_that_do_not_resolve_is_refused() {
            let state = scratch("sections-refused");
            let config = config();
            let unknown = SECTIONS.replace("#fuzz\n\nThen", "#nolabel\n\nThen");
            assert_eq!(
                write_plan(&state, &config, "demo", None, sections_plan(&unknown), 1),
                Err("#nolabel names no section of this plan".to_owned())
            );
            let missing = SECTIONS.replace("Depends on: #self", "Depends on: #self, #weida-4");
            assert_eq!(
                write_plan(&state, &config, "demo", None, sections_plan(&missing), 1),
                Err("no ticket #4 in weida".to_owned())
            );
            assert!(list(&state, &config, "demo").is_empty());
        }

        #[test]
        fn approving_a_plan_files_its_sections_once() {
            let state = scratch("sections");
            let config = config();
            let number =
                write_plan(&state, &config, "demo", None, sections_plan(SECTIONS), 1).unwrap();
            assert!(list(&state, &config, "weida").is_empty());
            set_state(&state, &config, "demo", number, Approved, 2).unwrap();

            let fuzz = load(&state, &config, "weida", 1).expect("filed in weida");
            assert_eq!(
                (fuzz.state, fuzz.title.as_str(), fuzz.source.as_str()),
                (Ready, "Keep the fuzz corpus", "demo")
            );
            assert_eq!(
                fuzz.body,
                "# Keep the fuzz corpus\nDepends on: #demo-1\n\nfuzz.sh restores the corpus.\n"
            );
            assert_eq!(
                fuzz.links,
                vec![DocLink {
                    kind: LinkKind::DependsOn,
                    number: 1,
                    repo: Some("demo".to_owned()),
                }]
            );
            let follow = load(&state, &config, "demo", 2).expect("filed in demo");
            assert_eq!(
                follow.body,
                "# Follow up\nDepends on: #weida-1\nRelated: #1\n\nThen this.\n"
            );
            assert_eq!(follow.state, Ready);
            let plan = load(&state, &config, "demo", number).unwrap();
            assert_eq!(
                plan.filed,
                vec![
                    FiledTicket {
                        repo: "weida".to_owned(),
                        number: 1,
                        title: "Keep the fuzz corpus".to_owned(),
                    },
                    FiledTicket {
                        repo: "demo".to_owned(),
                        number: 2,
                        title: "Follow up".to_owned(),
                    },
                ]
            );
            // The filed tickets wait for the plan's ticket.
            assert_eq!(fuzz.waits_for.len(), 1);

            // Back to review and approved again: nothing is filed twice.
            set_state(&state, &config, "demo", number, Review, 3).unwrap();
            set_state(&state, &config, "demo", number, Approved, 4).unwrap();
            assert_eq!(list(&state, &config, "weida").len(), 1);
            assert_eq!(list(&state, &config, "demo").len(), 2);
        }

        #[test]
        fn a_planning_ticket_without_a_plan_goes_back_with_a_note() {
            let state = scratch("planning-ended");
            let config = config();
            create(&state, &config, "demo", new(&text("a", &[])), 1).unwrap();
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
        fn a_ticket_keeps_its_report_and_a_stopped_master_its_last_words() {
            let state = scratch("report");
            let config = config();
            create(&state, &config, "demo", new(&text("a", &[])), 1).unwrap();
            planned(&state, &config, 1, &[], 2);
            set_state(&state, &config, "demo", 1, Approved, 3).unwrap();
            start(&state, &config, "demo", 1, vec!["x".into()], 4).unwrap();

            let ended = master_ended(
                &state,
                &config,
                "demo",
                1,
                MasterEnd::Stopped("exit 1"),
                Some("  stuck at step 2\n"),
                5,
            );
            assert_eq!(ended, Ok(true));
            let doc = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(doc.state, InProgress);
            assert_eq!(
                doc.report,
                Some(TicketReport {
                    text: "stuck at step 2".into(),
                    written: 5,
                    from_transcript: true,
                })
            );

            assert_eq!(
                finish(&state, &config, "demo", 1, " \n", 6),
                Err("a report is 1 to 65536 bytes".to_owned())
            );
            assert_eq!(state_of(&state, &config, 1), InProgress);

            finish(
                &state,
                &config,
                "demo",
                1,
                "# Done\n\nAll targets met.\n",
                7,
            )
            .unwrap();
            let doc = load(&state, &config, "demo", 1).unwrap();
            assert_eq!(doc.state, Done);
            assert_eq!(
                doc.report,
                Some(TicketReport {
                    text: "# Done\n\nAll targets met.".into(),
                    written: 7,
                    from_transcript: false,
                })
            );
            let listed = list(&state, &config, "demo");
            let report = listed[0].report.as_ref().expect("listed with its report");
            assert_eq!((report.text.as_str(), report.written), ("", 7));

            assert_eq!(
                finish(&state, &config, "demo", 1, "again", 8),
                Err("#1 is done; it cannot become done".to_owned())
            );
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
                let made = create(&state, &config, repo, new(&text("a", &[])), 1);
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
                body: "# join\n\nbody\n",
                source: "scan-rs",
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

        fn new_ticket(title: &str, depends_on: &[usize], auto_approve: bool) -> NewTicket {
            NewTicket {
                title: title.to_owned(),
                body: "proposal".to_owned(),
                depends_on: depends_on.to_vec(),
                auto_approve,
            }
        }

        #[test]
        fn an_onboarding_files_its_request_approved_and_its_tickets_ready_and_linked() {
            let state = scratch("onboard");
            let config = onboarding_config();
            let asked = spec("https://git.example.org/o/site");
            let tickets = [
                new_ticket("Jobs", &[], true),
                new_ticket("Deploy", &[0, 0], false),
            ];
            let made = onboard(&state, &config, "site", request(&asked), &tickets, 7);
            assert_eq!(made, Ok((1, vec![2, 3])));
            let doc = |n| load(&state, &config, "site", n).unwrap();
            assert_eq!((doc(1).kind, doc(1).state), (DocKind::Onboarding, Approved));
            assert_eq!((doc(2).state, doc(2).auto_approve), (Ready, true));
            assert_eq!(doc(3).links, depends(&[2]));
            assert!(!doc(3).auto_approve);
            // Pending: nothing is planned before the repository joins.
            assert!(ready(&state, &config).iter().all(|t| t.repo != "site"));
            assert_eq!(approved_onboardings(&state).len(), 1);

            let circle = [new_ticket("a", &[1], false), new_ticket("b", &[0], false)];
            assert_eq!(
                onboard(
                    &state,
                    &config,
                    "loop",
                    request(&spec("https://git.example.org/o/loop")),
                    &circle,
                    8
                ),
                Err("the onboarding's tickets 1 and 2 depend on each other".to_owned())
            );
            assert!(load(&state, &config, "loop", 1).is_none());
        }

        #[test]
        fn a_ticket_that_runs_through_is_approved_as_its_plan_arrives() {
            let state = scratch("auto");
            let config = config();
            create(&state, &config, "demo", new(&text("routine", &[])), 1).unwrap();
            let path = path_of(&docs_root(&state).join("demo"), 1);
            let mut doc: DocView = read_json(&path).unwrap();
            doc.auto_approve = true;
            write_json(&path, &doc).unwrap();
            planned(&state, &config, 1, &[], 5);
            let doc = load(&state, &config, "demo", 1).unwrap();
            assert_eq!((doc.state, doc.approved), (Approved, Some(5)));
            assert_eq!(queue(&state, &config).len(), 1);
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
            assert_eq!(
                create(&state, &config, "scan-rs", new(&text("p", &[])), 2),
                Ok(2)
            );
            assert_eq!(
                create(&state, &config, "scan-rs", new(&text("q", &[2])), 3),
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
