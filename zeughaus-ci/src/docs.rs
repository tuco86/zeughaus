//! Plans and proposals of each repository (see [`zeughaus_link::ci::DocView`]).
//!
//! `<state-dir>/ci/docs/<repo>/<number>.json` holds one document as the
//! view the editor shows, body included; `<state-dir>/ci/docs/<repo>/last`
//! holds the last number assigned, so a deleted document's number is never
//! given out again. Callers serialize the mutating functions (one lock in
//! the `/ci` server): numbering and the one-plan-in-progress rule rely on it.

use std::path::{Path, PathBuf};

use zeughaus_link::ci::{
    DocKind, DocState, DocView, MAX_DOC_BYTES, MAX_DOC_TITLE_BYTES, RepoDocsView, normalize_origin,
};

use crate::config::{CiConfig, valid_name};
use crate::files::{read_json, write_atomic, write_json};

/// The most proposals one plan may name.
const MAX_PLAN_PROPOSALS: usize = 32;

/// The longest `source` a document may carry.
const MAX_SOURCE_BYTES: usize = 100;

/// What [`create`] stores.
pub struct NewDoc<'a> {
    pub kind: DocKind,
    pub title: &'a str,
    pub body: &'a str,
    pub source: &'a str,
    pub proposals: &'a [u64],
}

/// Whether a document of `kind` may move from `from` to `to` on request.
/// `approved -> in_progress` is not among them: only [`claim`] does that.
pub fn allowed(kind: DocKind, from: DocState, to: DocState) -> bool {
    use DocState::*;
    match kind {
        DocKind::Proposal => matches!((from, to), (Open, Planned) | (Open, Dismissed)),
        DocKind::Plan => matches!(
            (from, to),
            (Saved, Approved)
                | (Saved, Rejected)
                | (Approved, Rejected)
                | (InProgress, Done)
                | (InProgress, Approved)
        ),
    }
}

/// The directory of `repo`'s documents. The name comes from a request and is
/// joined into a path, so only a repository of `ci.toml` gets one.
fn dir(state_dir: &Path, config: &CiConfig, repo: &str) -> Result<PathBuf, String> {
    if !valid_name(repo) || !config.repos.contains_key(repo) {
        return Err(format!("no repository {repo}"));
    }
    Ok(crate::ci_dir(state_dir).join("docs").join(repo))
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

/// Every document of `repo` without its body, in number order; nothing for
/// a repository that is not in `ci.toml`.
pub fn list(state_dir: &Path, config: &CiConfig, repo: &str) -> Vec<DocView> {
    let Ok(dir) = dir(state_dir, config, repo) else {
        return Vec::new();
    };
    let mut docs = read_all(&dir);
    for doc in &mut docs {
        doc.body.clear();
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

/// Stores a new document and answers its number. A proposal starts `open`,
/// a plan `saved`; the proposals a plan names become `planned` by it. A
/// refusal writes nothing.
pub fn create(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    new: NewDoc<'_>,
    now: u64,
) -> Result<u64, String> {
    let dir = dir(state_dir, config, repo)?;
    let title = new.title.trim();
    if title.is_empty() || title.len() > MAX_DOC_TITLE_BYTES || title.contains(['\n', '\r']) {
        return Err(format!(
            "a title is one line of 1 to {MAX_DOC_TITLE_BYTES} bytes"
        ));
    }
    if new.body.is_empty() || new.body.len() > MAX_DOC_BYTES {
        return Err(format!("a body is 1 to {MAX_DOC_BYTES} bytes"));
    }
    if !valid_source(new.source) {
        return Err(format!(
            "a source is 1 to {MAX_SOURCE_BYTES} of A-Z a-z 0-9 . _ -"
        ));
    }
    if new.kind == DocKind::Proposal && !new.proposals.is_empty() {
        return Err("only a plan names proposals".to_owned());
    }
    if new.proposals.len() > MAX_PLAN_PROPOSALS {
        return Err(format!(
            "a plan names at most {MAX_PLAN_PROPOSALS} proposals"
        ));
    }
    let mut taken = Vec::with_capacity(new.proposals.len());
    for &number in new.proposals {
        match read_json::<DocView>(&path_of(&dir, number)) {
            Ok(doc) if doc.kind == DocKind::Proposal && doc.state == DocState::Open => {
                if !taken.iter().any(|d: &DocView| d.number == number) {
                    taken.push(doc);
                }
            }
            Ok(doc) if doc.kind == DocKind::Proposal => {
                return Err(format!(
                    "proposal #{number} is {}; a plan takes up open proposals only",
                    doc.state.name()
                ));
            }
            Ok(_) => return Err(format!("#{number} is no proposal")),
            Err(_) => return Err(format!("no proposal #{number} in {repo}")),
        }
    }
    let number = last_number(&dir) + 1;
    let doc = DocView {
        repo: repo.to_owned(),
        number,
        kind: new.kind,
        title: title.to_owned(),
        state: match new.kind {
            DocKind::Proposal => DocState::Open,
            DocKind::Plan => DocState::Saved,
        },
        source: new.source.to_owned(),
        created: now,
        changed: now,
        approved: None,
        proposals: taken.iter().map(|doc| doc.number).collect(),
        plan: None,
        body: new.body.to_owned(),
    };
    write_json(&path_of(&dir, number), &doc)?;
    write_atomic(&dir.join("last"), number.to_string().as_bytes())?;
    for mut proposal in taken {
        proposal.state = DocState::Planned;
        proposal.plan = Some(number);
        proposal.changed = now;
        write_json(&path_of(&dir, proposal.number), &proposal)?;
    }
    Ok(number)
}

/// Moves document `number` to `to`, where [`allowed`] lets it. Approving a
/// plan stamps `approved`, which orders the queue; a requeued plan keeps
/// its stamp and with it its place.
pub fn set_state(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    number: u64,
    to: DocState,
    now: u64,
) -> Result<(), String> {
    if to == DocState::InProgress {
        return Err("only a master claims a plan".to_owned());
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
    if doc.state == DocState::Saved && to == DocState::Approved {
        doc.approved = Some(now);
    }
    doc.state = to;
    doc.changed = now;
    write_json(&path, &doc)
}

/// Deletes a document that is finished with: `planned`, `dismissed`,
/// `done` or `rejected`.
pub fn delete(state_dir: &Path, config: &CiConfig, repo: &str, number: u64) -> Result<(), String> {
    let dir = dir(state_dir, config, repo)?;
    let path = path_of(&dir, number);
    let doc: DocView = read_json(&path).map_err(|_| missing(repo, number))?;
    if !matches!(
        doc.state,
        DocState::Planned | DocState::Dismissed | DocState::Done | DocState::Rejected
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

/// The oldest approved plan of `repo` (by approval, then number) becomes
/// `in_progress` and comes back with its body; `None` when no plan is
/// approved. Refused while another plan is in progress: one master works in
/// a checkout at a time.
pub fn claim(
    state_dir: &Path,
    config: &CiConfig,
    repo: &str,
    now: u64,
) -> Result<Option<DocView>, String> {
    let dir = dir(state_dir, config, repo)?;
    let plans: Vec<DocView> = read_all(&dir)
        .into_iter()
        .filter(|doc| doc.kind == DocKind::Plan)
        .collect();
    if let Some(running) = plans.iter().find(|doc| doc.state == DocState::InProgress) {
        return Err(format!("plan #{} is in progress", running.number));
    }
    let Some(mut next) = plans
        .into_iter()
        .filter(|doc| doc.state == DocState::Approved)
        .min_by_key(|doc| (doc.approved.unwrap_or(0), doc.number))
    else {
        return Ok(None);
    };
    next.state = DocState::InProgress;
    next.changed = now;
    write_json(&path_of(&dir, next.number), &next)?;
    Ok(Some(next))
}

/// One line per repository of `ci.toml`, in name order: what waits for whom.
pub fn summary(state_dir: &Path, config: &CiConfig) -> Vec<RepoDocsView> {
    config
        .repos
        .iter()
        .map(|(name, repo)| {
            let docs = list(state_dir, config, name);
            let count = |kind: DocKind, state: DocState| {
                docs.iter()
                    .filter(|doc| doc.kind == kind && doc.state == state)
                    .count() as u32
            };
            RepoDocsView {
                repo: name.clone(),
                origin: normalize_origin(&repo.url),
                default_branch: repo.default_branch.clone(),
                awaiting: count(DocKind::Plan, DocState::Saved),
                queued: count(DocKind::Plan, DocState::Approved),
                running: docs
                    .iter()
                    .find(|doc| doc.kind == DocKind::Plan && doc.state == DocState::InProgress)
                    .map(|doc| doc.number),
                proposals: count(DocKind::Proposal, DocState::Open),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use DocState::*;

    #[test]
    fn only_the_transitions_of_the_table_are_allowed() {
        let plan = |from, to| allowed(DocKind::Plan, from, to);
        let proposal = |from, to| allowed(DocKind::Proposal, from, to);
        assert!(proposal(Open, Planned));
        assert!(proposal(Open, Dismissed));
        assert!(plan(Saved, Approved));
        assert!(plan(Saved, Rejected));
        assert!(plan(Approved, Rejected));
        assert!(plan(InProgress, Done));
        assert!(plan(InProgress, Approved));

        assert!(!plan(Approved, InProgress));
        assert!(!plan(Saved, Done));
        assert!(!plan(Done, Approved));
        assert!(!plan(Rejected, Approved));
        assert!(!plan(Open, Planned));
        assert!(!proposal(Planned, Open));
        assert!(!proposal(Dismissed, Planned));
        assert!(!proposal(Saved, Approved));
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

        fn new<'a>(kind: DocKind, title: &'a str, proposals: &'a [u64]) -> NewDoc<'a> {
            NewDoc {
                kind,
                title,
                body: "body",
                source: "weida",
                proposals,
            }
        }

        fn state_of(state: &Path, config: &CiConfig, number: u64) -> DocState {
            load(state, config, "demo", number).expect("stored").state
        }

        #[test]
        fn numbers_rise_across_kinds_and_a_deleted_one_is_not_reused() {
            let state = scratch("numbers");
            let config = config();
            let a = create(&state, &config, "demo", new(DocKind::Proposal, "a", &[]), 1);
            let b = create(&state, &config, "demo", new(DocKind::Plan, "b", &[]), 2);
            assert_eq!((a, b), (Ok(1), Ok(2)));
            set_state(&state, &config, "demo", 2, Rejected, 3).expect("reject");
            delete(&state, &config, "demo", 2).expect("delete");
            let c = create(&state, &config, "demo", new(DocKind::Plan, "c", &[]), 4);
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
        fn a_plan_takes_up_the_open_proposals_it_names_and_only_those() {
            let state = scratch("takes-up");
            let config = config();
            create(
                &state,
                &config,
                "demo",
                new(DocKind::Proposal, "p1", &[]),
                1,
            )
            .unwrap();
            create(
                &state,
                &config,
                "demo",
                new(DocKind::Proposal, "p2", &[]),
                1,
            )
            .unwrap();
            set_state(&state, &config, "demo", 2, Dismissed, 2).unwrap();

            let refused = create(&state, &config, "demo", new(DocKind::Plan, "x", &[1, 2]), 3);
            assert!(refused.unwrap_err().contains("#2"));
            let unknown = create(&state, &config, "demo", new(DocKind::Plan, "x", &[9]), 3);
            assert_eq!(unknown, Err("no proposal #9 in demo".to_owned()));
            // Nothing written: no plan, #1 still open.
            assert_eq!(list(&state, &config, "demo").len(), 2);
            assert_eq!(state_of(&state, &config, 1), Open);

            let plan = create(&state, &config, "demo", new(DocKind::Plan, "x", &[1]), 4);
            assert_eq!(plan, Ok(3));
            let proposal = load(&state, &config, "demo", 1).unwrap();
            assert_eq!((proposal.state, proposal.plan), (Planned, Some(3)));
            assert_eq!(load(&state, &config, "demo", 3).unwrap().proposals, vec![1]);
        }

        #[test]
        fn a_claim_takes_the_oldest_approved_plan_one_at_a_time() {
            let state = scratch("claim");
            let config = config();
            for title in ["a", "b", "c"] {
                create(&state, &config, "demo", new(DocKind::Plan, title, &[]), 1).unwrap();
            }
            assert_eq!(claim(&state, &config, "demo", 2), Ok(None));
            // #2 approved before #1: the queue follows approval.
            set_state(&state, &config, "demo", 2, Approved, 10).unwrap();
            set_state(&state, &config, "demo", 1, Approved, 20).unwrap();
            assert_eq!(
                set_state(&state, &config, "demo", 1, InProgress, 21),
                Err("only a master claims a plan".to_owned())
            );

            let first = claim(&state, &config, "demo", 30).unwrap().expect("a plan");
            assert_eq!((first.number, first.state), (2, InProgress));
            assert_eq!(first.body, "body");
            assert_eq!(
                claim(&state, &config, "demo", 31),
                Err("plan #2 is in progress".to_owned())
            );
            assert_eq!(
                delete(&state, &config, "demo", 1),
                Err("#1 is approved; only a finished document is deleted".to_owned())
            );
            set_state(&state, &config, "demo", 2, Done, 32).unwrap();
            let second = claim(&state, &config, "demo", 33).unwrap().expect("a plan");
            assert_eq!(second.number, 1);
        }

        #[test]
        fn a_repository_outside_ci_toml_reads_and_writes_nothing() {
            let state = scratch("outside");
            let config = config();
            for repo in ["other", "../x"] {
                let made = create(&state, &config, repo, new(DocKind::Proposal, "a", &[]), 1);
                assert_eq!(made, Err(format!("no repository {repo}")));
                assert!(list(&state, &config, repo).is_empty());
                assert!(claim(&state, &config, repo, 1).is_err());
            }
            assert!(!state.join("ci").exists());
            assert!(!state.join("x").exists());
        }
    }
}
