//! `zeughaus-runner tidy [--dry-run] [--list]`: keeps the checkouts of the
//! CI's repositories on this host in shape. A maintenance task runs it.
//!
//! The repositories are the ones the CI lists. For each, the checkouts are
//! the user's work checkout `<work root>/<origin>` with its linked
//! worktrees, and the agent sessions' plan and master clones under
//! `<state-dir>/agent-work/`. For each checkout it
//!
//! - fetches (not a linked worktree: it shares its main checkout's refs);
//! - fast-forwards the work checkout's default branch when the checkout is
//!   clean and the branch has nothing unpushed;
//! - removes a cargo `target/` no build has touched for 24 h;
//! - removes the checkout once nothing happened in it for 72 h, and only
//!   when nothing would be lost: no changed, untracked or ignored file, stash
//!   entry, unpushed branch, tag or HEAD, no linked worktree, and for the
//!   master clone no plan clone that borrows its objects.
//!
//! A checkout a session holds (its `.git/zeughaus-session.lock`) is skipped,
//! and so is a `target/` cargo is building in. Its own reflog entries are
//! marked `zeughaus-tidy` and do not count as activity. `--list` prints the
//! repositories' origins, one per line, for tools that must leave these
//! checkouts to `tidy`.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zeughaus_link::{CiReply, CiRequest, RepoDocsView, normalize_origin};

use crate::agents::{SESSION_LOCK, checkout_of, master_dir, plan_clone_dir, work_root};
use crate::mcp::client::{Client, Link};

/// How long a cargo `target/` may go without a build.
const TARGET_IDLE: Duration = Duration::from_secs(24 * 3_600);
/// How long a checkout may go without activity before it is removed.
const CHECKOUT_IDLE: Duration = Duration::from_secs(72 * 3_600);
/// What tidy's own reflog entries start with.
const REFLOG_ACTION: &str = "zeughaus-tidy";
/// The first line of the `CACHEDIR.TAG` cargo writes into its `target/`.
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";
const USAGE: &str = "usage: zeughaus-runner tidy [--dry-run] [--list]";

pub fn run(args: &[String], state_dir: &Path) -> ExitCode {
    let (mut dry_run, mut list) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--list" => list = true,
            other => {
                eprintln!("[tidy] unexpected argument {other:?}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let repos = match ci_repos(state_dir) {
        Ok(repos) => repos,
        Err(e) => {
            println!("[tidy] no answer from the CI: {e}; nothing done");
            return ExitCode::from(2);
        }
    };
    if list {
        for repo in &repos {
            println!("{}", repo.origin);
        }
        return ExitCode::SUCCESS;
    }
    let work = work_root();
    let mut tidy = Tidy::new(SystemTime::now(), dry_run);
    for repo in &repos {
        let targets = targets(&repo.origin, state_dir, work.as_deref());
        tidy.repo(&targets, &repo.default_branch);
    }
    if tidy.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The repositories of the CI's `ci.toml`.
fn ci_repos(state_dir: &Path) -> Result<Vec<RepoDocsView>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let client = Client::new(Link::resolve(None, state_dir));
    let asked = runtime.block_on(async {
        let asked = client.ci(CiRequest::Overview).await;
        client.shutdown().await;
        asked
    });
    match asked? {
        CiReply::Overview { overview } => Ok(overview.repos),
        other => Err(crate::mcp::unexpected(&other)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The user's checkout under the work root.
    Work,
    /// A linked worktree of a work checkout.
    Worktree,
    /// The plan session's clone.
    Plan,
    /// The master session's clone.
    Master,
}

struct Target {
    path: PathBuf,
    kind: Kind,
    /// Where removing stops pruning empty parent directories.
    root: PathBuf,
    /// The main checkout of a linked worktree, which removes it.
    main: Option<PathBuf>,
    /// The plan clone that borrows a master clone's objects.
    plan: Option<PathBuf>,
}

/// The checkouts of `origin` in the order they are worked on: a linked
/// worktree blocks its main checkout, and the plan clone the master clone.
fn targets(origin: &str, state_dir: &Path, work: Option<&Path>) -> Vec<Target> {
    let mut targets = Vec::new();
    if let Some(root) = work
        && let Ok(checkout) = checkout_of(root, origin)
        && is_checkout_of(&checkout, origin)
    {
        for linked in worktrees(&checkout).into_iter().skip(1) {
            if linked.path.is_dir() {
                targets.push(Target {
                    path: linked.path,
                    kind: Kind::Worktree,
                    root: root.to_owned(),
                    main: Some(checkout.clone()),
                    plan: None,
                });
            }
        }
        targets.push(Target {
            path: checkout,
            kind: Kind::Work,
            root: root.to_owned(),
            main: None,
            plan: None,
        });
    }
    let agents = crate::agents::agent_root(state_dir);
    let plan = plan_clone_dir(state_dir, origin).ok();
    if let Some(plan) = &plan
        && plan.join(".git").is_dir()
    {
        targets.push(Target {
            path: plan.clone(),
            kind: Kind::Plan,
            root: agents.clone(),
            main: None,
            plan: None,
        });
    }
    if let Ok(master) = master_dir(state_dir, origin)
        && master.join(".git").is_dir()
    {
        targets.push(Target {
            path: master,
            kind: Kind::Master,
            root: agents,
            main: None,
            plan,
        });
    }
    targets
}

/// A checkout of its own (not a worktree, not a mirror guard file) whose
/// `origin` is `origin`.
fn is_checkout_of(path: &Path, origin: &str) -> bool {
    path.join(".git").is_dir()
        && git(path, &["remote", "get-url", "origin"])
            .is_ok_and(|url| normalize_origin(url.trim()) == origin)
}

struct Worktree {
    path: PathBuf,
    /// The branch checked out in it, `refs/heads/<name>`.
    branch: Option<String>,
}

/// Every worktree of `checkout`, the main one first.
fn worktrees(checkout: &Path) -> Vec<Worktree> {
    git(checkout, &["worktree", "list", "--porcelain"])
        .map(|list| parse_worktrees(&list))
        .unwrap_or_default()
}

fn parse_worktrees(list: &str) -> Vec<Worktree> {
    let mut found: Vec<Worktree> = Vec::new();
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            found.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
            });
        } else if let Some(branch) = line.strip_prefix("branch ")
            && let Some(last) = found.last_mut()
        {
            last.branch = Some(branch.to_owned());
        }
    }
    found
}

/// One pass over the targets of the CI's repositories.
struct Tidy {
    now: SystemTime,
    dry_run: bool,
    /// An `error` line was printed.
    failed: bool,
    /// Every line printed, without the `[tidy] ` prefix.
    said: Vec<String>,
}

impl Tidy {
    fn new(now: SystemTime, dry_run: bool) -> Tidy {
        Tidy {
            now,
            dry_run,
            failed: false,
            said: Vec::new(),
        }
    }

    fn say(&mut self, line: String) {
        println!("[tidy] {line}");
        self.said.push(line);
    }

    fn error(&mut self, line: String) {
        self.failed = true;
        self.say(format!("error {line}"));
    }

    /// `verb`, or `would <verb>` in a dry run.
    fn act(&self, verb: &str) -> String {
        if self.dry_run {
            format!("would {verb}")
        } else {
            verb.to_owned()
        }
    }

    fn repo(&mut self, targets: &[Target], branch: &str) {
        let mut fetched = true;
        for target in targets {
            self.target(target, branch, &mut fetched);
        }
    }

    /// Works on one checkout. `fetched` turns false at the repository's
    /// first failed fetch; from then on nothing of it is removed.
    fn target(&mut self, target: &Target, branch: &str, fetched: &mut bool) {
        let path = &target.path;
        let shown = path.display().to_string();
        let dirs = git(path, &["rev-parse", "--absolute-git-dir"]).and_then(|git_dir| {
            let common = git(
                path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?;
            Ok((PathBuf::from(git_dir.trim()), PathBuf::from(common.trim())))
        });
        let (git_dir, common) = match dirs {
            Ok(dirs) => dirs,
            Err(e) => return self.error(format!("{shown}: {e}")),
        };
        let _session = match hold([common.join(SESSION_LOCK)]) {
            Ok(Lock::Held(files)) => files,
            Ok(Lock::Busy) => return self.say(format!("skip {shown}: a session uses it")),
            Err(e) => return self.error(format!("{shown}: {e}")),
        };
        if target.kind != Kind::Worktree
            && !self.dry_run
            && let Err(e) = fetch(path)
        {
            self.error(format!("{shown}: fetch failed: {e}"));
            *fetched = false;
        }
        if target.kind == Kind::Work {
            self.fast_forward(path, branch);
        }
        let (purged, built) = self.sweep(path);
        let activity = last_activity_of(&git_dir).max(built);
        if !*fetched || !due(activity, self.now, CHECKOUT_IDLE) {
            return;
        }
        match removal_blocker(target, &purged) {
            Some(blocker) => self.say(format!("kept {shown}: {blocker}")),
            None => self.remove(target, activity),
        }
    }

    /// Brings the work checkout's `branch` to `origin/<branch>` when that
    /// loses nothing: checked out in a clean tree with `git merge
    /// --ff-only`, otherwise by moving the ref, unless a linked worktree
    /// has it checked out.
    fn fast_forward(&mut self, path: &Path, branch: &str) {
        let shown = path.display().to_string();
        let local = format!("refs/heads/{branch}");
        let remote = format!("refs/remotes/origin/{branch}");
        let (Ok(old), Ok(new)) = (commit(path, &local), commit(path, &remote)) else {
            return;
        };
        if old == new {
            return;
        }
        match git(path, &["status", "--porcelain", "--untracked-files=no"]) {
            Ok(status) if status.trim().is_empty() => {}
            Ok(_) => return self.say(format!("skip {shown}: dirty")),
            Err(e) => return self.error(format!("{shown}: {e}")),
        }
        let counted = count(path, &format!("{new}..{old}"))
            .and_then(|ahead| Ok((ahead, count(path, &format!("{old}..{new}"))?)));
        let (ahead, behind) = match counted {
            Ok(counted) => counted,
            Err(e) => return self.error(format!("{shown}: {e}")),
        };
        if ahead > 0 {
            return self.say(format!("skip {shown}: {ahead} ahead, {behind} behind"));
        }
        let head = git(path, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
        let checked_out = head.trim() == local;
        if !checked_out
            && worktrees(path)
                .iter()
                .any(|worktree| worktree.branch.as_deref() == Some(local.as_str()))
        {
            return;
        }
        if !self.dry_run {
            let moved = if checked_out {
                git_with(
                    path,
                    &["merge", "--ff-only", "-q", &format!("origin/{branch}")],
                    &[("GIT_REFLOG_ACTION", REFLOG_ACTION)],
                )
            } else {
                let message = format!("{REFLOG_ACTION}: fast-forward");
                git(path, &["update-ref", "-m", &message, &local, &new, &old])
            };
            if let Err(e) = moved {
                return self.error(format!("{shown}: fast-forward failed: {e}"));
            }
        }
        self.say(format!(
            "{} {shown}: {behind} commits",
            self.act("fast-forward")
        ));
    }

    /// Removes the checkout's cargo `target/` when no build used it for
    /// `TARGET_IDLE`. Answers what it removed (or would remove) and when
    /// the last build was.
    fn sweep(&mut self, path: &Path) -> (Vec<PathBuf>, Option<SystemTime>) {
        let target = path.join("target");
        let cargo_made = std::fs::read_to_string(target.join("CACHEDIR.TAG"))
            .is_ok_and(|tag| tag.starts_with(CACHEDIR_SIGNATURE));
        if !target.is_dir()
            || !cargo_made
            || !git(path, &["ls-files", "--", "target"]).is_ok_and(|files| files.trim().is_empty())
        {
            return (Vec::new(), None);
        }
        let built = last_build(&target);
        let shown = target.display().to_string();
        let locks = build_dirs(&target)
            .into_iter()
            .map(|dir| dir.join(".cargo-lock"));
        let _builds = match hold(locks) {
            Ok(Lock::Held(files)) => files,
            Ok(Lock::Busy) => return (Vec::new(), built),
            Err(e) => {
                self.error(format!("{shown}: {e}"));
                return (Vec::new(), built);
            }
        };
        if !due(built, self.now, TARGET_IDLE) {
            return (Vec::new(), built);
        }
        if !self.dry_run
            && let Err(e) = std::fs::remove_dir_all(&target)
        {
            self.error(format!("{shown}: cannot remove: {e}"));
            return (Vec::new(), built);
        }
        self.say(format!(
            "{} {shown}: idle {}",
            self.act("remove-target"),
            idle(built, self.now)
        ));
        (vec![target], built)
    }

    fn remove(&mut self, target: &Target, activity: Option<SystemTime>) {
        let shown = target.path.display().to_string();
        if !self.dry_run {
            let removed = match &target.main {
                Some(main) if target.kind == Kind::Worktree => {
                    git(main, &["worktree", "remove", &shown]).map(drop)
                }
                _ => std::fs::remove_dir_all(&target.path).map_err(|e| e.to_string()),
            };
            if let Err(e) = removed {
                return self.error(format!("{shown}: cannot remove: {e}"));
            }
            prune_parents(&target.path, &target.root);
        }
        self.say(format!(
            "{} {shown}: idle {}, clean and fully pushed",
            self.act("remove"),
            idle(activity, self.now)
        ));
    }
}

/// Why removing the target would lose something, or `None`. `purged` are
/// the `target/` directories that are (or in a dry run would be) removed:
/// they do not count as ignored files.
fn removal_blocker(target: &Target, purged: &[PathBuf]) -> Option<String> {
    let path = &target.path;
    let skip: Vec<String> = purged
        .iter()
        .filter_map(|purged| purged.strip_prefix(path).ok())
        .map(|relative| relative.to_string_lossy().into_owned())
        .collect();
    let status = match git(
        path,
        &[
            "status",
            "--porcelain",
            "-z",
            "--ignored",
            "--untracked-files=all",
        ],
    ) {
        Ok(status) => status,
        Err(e) => return Some(format!("git status failed: {e}")),
    };
    for entry in status.split('\0').filter(|entry| !entry.is_empty()) {
        let ignored = entry.starts_with("!! ");
        let name = entry.get(3..).unwrap_or(entry).trim_end_matches('/');
        if ignored
            && skip
                .iter()
                .any(|skip| name == skip || name.starts_with(&format!("{skip}/")))
        {
            continue;
        }
        let what = if ignored {
            "ignored"
        } else {
            "uncommitted or untracked"
        };
        return Some(format!("{what} file {name}"));
    }
    // A linked worktree shares the stash, branches and tags of its main
    // checkout; they are checked with it, and removing the worktree keeps
    // them.
    if target.kind != Kind::Worktree {
        match git(path, &["stash", "list"]) {
            Ok(list) if list.trim().is_empty() => {}
            Ok(_) => return Some("stash entries".to_owned()),
            Err(e) => return Some(format!("git stash list failed: {e}")),
        }
        let heads = match git(
            path,
            &[
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                "refs/heads",
            ],
        ) {
            Ok(heads) => heads,
            Err(e) => return Some(format!("git for-each-ref failed: {e}")),
        };
        for (name, sha) in heads.lines().filter_map(|line| line.split_once(' ')) {
            if !on_remote(path, sha) {
                return Some(format!("branch {name} is not on a remote"));
            }
        }
    }
    if let Ok(head) = commit(path, "HEAD")
        && !on_remote(path, &head)
    {
        return Some("HEAD is not on a remote branch".to_owned());
    }
    if target.kind != Kind::Worktree
        && let Some(blocker) = unpushed_tag(path)
    {
        return Some(blocker);
    }
    if target.kind == Kind::Work && worktrees(path).len() > 1 {
        return Some("linked worktrees".to_owned());
    }
    if target.kind == Kind::Master && target.plan.as_ref().is_some_and(|plan| plan.exists()) {
        return Some("its plan clone still exists".to_owned());
    }
    None
}

/// The first tag that the remote does not have with the same object.
fn unpushed_tag(path: &Path) -> Option<String> {
    let local = match git(
        path,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/tags",
        ],
    ) {
        Ok(local) => local,
        Err(e) => return Some(format!("git for-each-ref failed: {e}")),
    };
    if local.trim().is_empty() {
        return None;
    }
    let remote = match git_with(path, &["ls-remote", "--tags", "origin"], &remote_env()) {
        Ok(remote) => remote,
        Err(e) => return Some(format!("the remote's tags are unknown: {e}")),
    };
    let remote: HashMap<&str, &str> = remote
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(sha, name)| (name, sha))
        .collect();
    local
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(name, sha)| remote.get(name) != Some(sha))
        .map(|(name, _)| {
            format!(
                "tag {} is not on the remote",
                name.trim_start_matches("refs/tags/")
            )
        })
}

/// Whether a remote-tracking branch contains `sha`.
fn on_remote(path: &Path, sha: &str) -> bool {
    git(
        path,
        &[
            "for-each-ref",
            "--contains",
            sha,
            "--count=1",
            "refs/remotes",
        ],
    )
    .is_ok_and(|found| !found.trim().is_empty())
}

/// Removes the empty directories above `path`, up to but not including
/// `root`.
fn prune_parents(path: &Path, root: &Path) {
    let mut dir = path.parent();
    while let Some(parent) = dir {
        if parent == root || !parent.starts_with(root) || std::fs::remove_dir(parent).is_err() {
            return;
        }
        dir = parent.parent();
    }
}

enum Lock {
    /// Every lock that exists is held here until the files are dropped.
    Held(Vec<File>),
    /// Someone else holds one of them.
    Busy,
}

/// Takes the exclusive lock of each file that exists, without waiting.
fn hold(paths: impl IntoIterator<Item = PathBuf>) -> Result<Lock, String> {
    let mut held = Vec::new();
    for path in paths {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
        };
        match file.try_lock() {
            Ok(()) => held.push(file),
            Err(TryLockError::WouldBlock) => return Ok(Lock::Busy),
            Err(TryLockError::Error(e)) => {
                return Err(format!("cannot lock {}: {e}", path.display()));
            }
        }
    }
    Ok(Lock::Held(held))
}

/// The directories a build of `target/` writes into: its children and
/// grandchildren (`debug`, `<triple>/debug`, `agent/debug`).
fn build_dirs(target: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for child in subdirs(target) {
        dirs.extend(subdirs(&child));
        dirs.push(child);
    }
    dirs
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// When a build last wrote into `target/`: the newest modification time of
/// the directory and of each build directory with its lock file, `deps`,
/// `.fingerprint`, `incremental` and `build`.
fn last_build(target: &Path) -> Option<SystemTime> {
    let mut newest = modified(target);
    for dir in build_dirs(target) {
        newest = newest.max(modified(&dir));
        for name in [
            ".cargo-lock",
            "deps",
            ".fingerprint",
            "incremental",
            "build",
        ] {
            newest = newest.max(modified(&dir.join(name)));
        }
    }
    newest
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// When something last happened in the checkout of `git_dir`: its newest
/// reflog entry that tidy did not make, else the modification of `HEAD`.
fn last_activity_of(git_dir: &Path) -> Option<SystemTime> {
    std::fs::read_to_string(git_dir.join("logs").join("HEAD"))
        .ok()
        .and_then(|reflog| last_activity(&reflog))
        .map(|epoch| UNIX_EPOCH + Duration::from_secs(epoch))
        .or_else(|| modified(&git_dir.join("HEAD")))
}

/// The newest epoch of a reflog's lines (`<old> <new> <ident> <epoch>
/// <tz>\t<message>`) whose message tidy did not write.
fn last_activity(reflog: &str) -> Option<u64> {
    reflog
        .lines()
        .filter_map(|line| {
            let (entry, message) = line.split_once('\t').unwrap_or((line, ""));
            if message.starts_with(REFLOG_ACTION) {
                return None;
            }
            let mut tokens = entry.split_whitespace().rev();
            tokens.next()?;
            tokens.next()?.parse().ok()
        })
        .max()
}

/// Whether `last` is longer than `idle` before `now`; never seen is due.
fn due(last: Option<SystemTime>, now: SystemTime, idle: Duration) -> bool {
    last.is_none_or(|last| now.duration_since(last).is_ok_and(|age| age > idle))
}

fn idle(last: Option<SystemTime>, now: SystemTime) -> String {
    last.and_then(|last| now.duration_since(last).ok())
        .map_or_else(
            || "unknown".to_owned(),
            |age| crate::maintain::age(age.as_secs()),
        )
}

/// The object `rev` names as a commit.
fn commit(path: &Path, rev: &str) -> Result<String, String> {
    git(
        path,
        &["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")],
    )
    .map(|sha| sha.trim().to_owned())
}

fn count(path: &Path, range: &str) -> Result<u64, String> {
    let counted = git(path, &["rev-list", "--count", range])?;
    counted
        .trim()
        .parse()
        .map_err(|_| format!("git rev-list --count {range} answered {counted:?}"))
}

/// What git talks to the remote with: never a prompt, an ssh that gives up.
fn remote_env() -> Vec<(&'static str, &'static str)> {
    if std::env::var_os("GIT_SSH_COMMAND").is_some() {
        Vec::new()
    } else {
        vec![(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15",
        )]
    }
}

fn fetch(path: &Path) -> Result<(), String> {
    git_with(
        path,
        &["fetch", "-q", "--prune", "--tags", "origin"],
        &remote_env(),
    )
    .map(drop)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_with(dir, args, &[])
}

/// Runs git in `dir` without a prompt; `Err` is its last line of stderr.
fn git_with(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3_600);

    #[test]
    fn activity_is_the_newest_reflog_entry_tidy_did_not_make() {
        let reflog = "\
0000 1111 A <a@x> 1000 +0100\tclone: from /o
1111 2222 A <a@x> 3000 +0100\tzeughaus-tidy: Fast-forward
2222 3333 A <a@x> 2000 +0100\tcheckout: moving from main to x
";
        assert_eq!(last_activity(reflog), Some(2000));
        assert_eq!(last_activity(""), None);
        assert_eq!(
            last_activity("1111 2222 A <a@x> 3000 +0100\tzeughaus-tidy: fast-forward\n"),
            None
        );
    }

    #[test]
    fn a_checkout_is_due_once_it_was_idle_longer_than_the_limit() {
        let now = UNIX_EPOCH + 1_000 * HOUR;
        let idle = 24 * HOUR;
        let second = Duration::from_secs(1);
        assert!(due(Some(now - idle - second), now, idle));
        assert!(!due(Some(now - idle + second), now, idle));
        assert!(due(None, now, idle));
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        fn run(dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=ci", "-c", "user.email=ci@example.invalid"])
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn commit_file(dir: &Path, name: &str) {
            std::fs::write(dir.join(name), name).unwrap();
            run(dir, &["add", name]);
            run(dir, &["commit", "-q", "-m", name]);
        }

        /// A bare origin with one commit on `main`, and a clone of it.
        struct Repo {
            base: PathBuf,
            origin: PathBuf,
            clone: PathBuf,
        }

        impl Repo {
            fn new(name: &str) -> Repo {
                let base =
                    std::env::temp_dir().join(format!("zh-tidy-{}-{name}", std::process::id()));
                let _ = std::fs::remove_dir_all(&base);
                std::fs::create_dir_all(&base).unwrap();
                let origin = base.join("origin.git");
                run(
                    &base,
                    &[
                        "init",
                        "-q",
                        "--bare",
                        "-b",
                        "main",
                        origin.to_str().unwrap(),
                    ],
                );
                let seed = base.join("seed");
                run(&base, &["clone", "-q", origin.to_str().unwrap(), "seed"]);
                run(&seed, &["symbolic-ref", "HEAD", "refs/heads/main"]);
                commit_file(&seed, "a");
                run(&seed, &["push", "-q", "origin", "main"]);
                let clone = base.join("h").join("o").join("r");
                run(&base, &["clone", "-q", origin.to_str().unwrap(), "h/o/r"]);
                Repo {
                    base,
                    origin,
                    clone,
                }
            }

            fn target(&self, kind: Kind) -> Target {
                Target {
                    path: self.clone.clone(),
                    kind,
                    root: self.base.clone(),
                    main: None,
                    plan: None,
                }
            }

            /// One more commit on the origin's `main`, made from the seed.
            fn advance_origin(&self) {
                let seed = self.base.join("seed");
                commit_file(&seed, "b");
                run(&seed, &["push", "-q", "origin", "main"]);
            }
        }

        impl Drop for Repo {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.base);
            }
        }

        fn pass(target: &Target, now: SystemTime) -> Tidy {
            let mut tidy = Tidy::new(now, false);
            let mut fetched = true;
            tidy.target(target, "main", &mut fetched);
            assert!(!tidy.failed, "{:?}", tidy.said);
            tidy
        }

        #[test]
        fn a_branch_that_was_never_pushed_keeps_the_checkout() {
            let repo = Repo::new("unpushed");
            run(&repo.clone, &["checkout", "-q", "-b", "topic"]);
            commit_file(&repo.clone, "t");
            run(&repo.clone, &["checkout", "-q", "main"]);
            let tidy = pass(&repo.target(Kind::Plan), SystemTime::now() + 73 * HOUR);
            assert!(repo.clone.is_dir());
            assert!(
                tidy.said
                    .iter()
                    .any(|line| line.ends_with(": branch topic is not on a remote")),
                "{:?}",
                tidy.said
            );
        }

        #[test]
        fn a_clean_pushed_checkout_goes_after_72_hours_idle() {
            let repo = Repo::new("removed");
            let now = SystemTime::now();
            pass(&repo.target(Kind::Plan), now + 71 * HOUR);
            assert!(repo.clone.is_dir());
            pass(&repo.target(Kind::Plan), now + 73 * HOUR);
            assert!(!repo.clone.exists());
            // The empty parents up to the root go with it.
            assert!(!repo.base.join("h").exists());
            assert!(repo.origin.is_dir());
        }

        #[test]
        fn a_clean_checkout_behind_its_origin_is_fast_forwarded() {
            let repo = Repo::new("forward");
            repo.advance_origin();
            pass(&repo.target(Kind::Work), SystemTime::now());
            assert_eq!(
                run(&repo.clone, &["rev-parse", "HEAD"]),
                run(&repo.origin, &["rev-parse", "main"])
            );
            let reflog = std::fs::read_to_string(repo.clone.join(".git/logs/HEAD")).unwrap();
            let newest = reflog.lines().last().unwrap();
            let message = newest.split_once('\t').unwrap().1;
            assert!(message.starts_with(REFLOG_ACTION), "{newest}");
        }

        #[test]
        fn a_checkout_with_a_changed_file_is_not_fast_forwarded() {
            let repo = Repo::new("dirty");
            let before = run(&repo.clone, &["rev-parse", "HEAD"]);
            repo.advance_origin();
            std::fs::write(repo.clone.join("a"), "changed").unwrap();
            let tidy = pass(&repo.target(Kind::Work), SystemTime::now());
            assert_eq!(run(&repo.clone, &["rev-parse", "HEAD"]), before);
            assert!(tidy.said.iter().any(|line| line.ends_with(": dirty")));
        }

        #[test]
        fn a_target_cargo_is_building_in_stays() {
            let repo = Repo::new("target");
            let target = repo.clone.join("target");
            std::fs::create_dir_all(target.join("debug")).unwrap();
            std::fs::write(
                target.join("CACHEDIR.TAG"),
                format!("{CACHEDIR_SIGNATURE}\n"),
            )
            .unwrap();
            let lock_path = target.join("debug").join(".cargo-lock");
            std::fs::write(&lock_path, "").unwrap();
            let build = File::open(&lock_path).unwrap();
            build.try_lock().expect("the build's lock");
            let later = SystemTime::now() + 25 * HOUR;
            pass(&repo.target(Kind::Plan), later);
            assert!(target.is_dir());
            drop(build);
            pass(&repo.target(Kind::Plan), later);
            assert!(!target.exists());
        }
    }
}
