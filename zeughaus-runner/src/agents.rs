//! Agent sessions the CI opens on this runner, each in a terminal of the
//! runner's mux:
//!
//! - `plan <repo> [--proposal <n>]`: omp in plan mode in the plan clone, a
//!   checkout detached at the repository's default branch that this keeps
//!   clean and current. It borrows the master clone's objects and never
//!   builds. With a proposal, omp starts planning that open proposal of the
//!   CI at once. Its terminal opens as a tab of the locked `Triggered`
//!   group.
//! - `master <repo> --plan <n>`: omp executing plan `n`, which the CI put in
//!   progress on this runner, in the master clone, fast-forwarded to the
//!   default branch first. Its terminal stays detached.
//!
//! Both clones live in the runner's state directory, never among the user's
//! own checkouts: `<state-dir>/agent-work/{master,plan}/<host>/<owner>/<repo>`.
//! A missing one is cloned over ssh from the origin in the session's terminal
//! before the session starts. While a session runs it holds a shared lock on
//! the clone's `.git/zeughaus-session.lock`, so `tidy` leaves the clone alone.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(not(unix))]
use std::process::ExitCode;

/// The directory checkouts live under: `$ZEUGHAUS_WORK`, else `~/work`.
#[cfg(unix)]
pub fn work_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("ZEUGHAUS_WORK").filter(|root| !root.is_empty()) {
        return Some(PathBuf::from(root));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join("work"))
}

/// The checkout of `origin` (`host/owner/name`, as `normalize_origin` gives
/// it) under `root`. The origin comes from a peer, so it must be a relative
/// path of at least three plain segments.
#[cfg(unix)]
pub fn checkout_of(root: &Path, origin: &str) -> Result<PathBuf, String> {
    let refused = || format!("{origin:?} names no checkout under the work root");
    if origin.is_empty()
        || origin.starts_with('/')
        || !origin.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        return Err(refused());
    }
    let segments: Vec<&str> = origin.split('/').collect();
    if segments.len() < 3
        || segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(refused());
    }
    Ok(root.join(origin))
}

/// The ssh URL `origin` (`host/owner/name`, as `checkout_of` accepts it) is
/// cloned from: scp form, or an `ssh://` URL when the host names a port.
#[cfg(unix)]
fn clone_url(origin: &str) -> String {
    let (host, rest) = origin.split_once('/').unwrap_or((origin, ""));
    if host.contains(':') {
        format!("ssh://git@{host}/{rest}.git")
    } else {
        format!("git@{host}:{rest}.git")
    }
}

/// The directory the agent sessions' clones live under.
#[cfg(unix)]
pub fn agent_root(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-work")
}

/// The clone the master session executes the plans of `origin` in.
#[cfg(unix)]
pub fn master_dir(state_dir: &Path, origin: &str) -> Result<PathBuf, String> {
    checkout_of(&agent_root(state_dir).join("master"), origin)
}

/// The clone plans of `origin` are made in; it borrows the master clone's
/// objects.
#[cfg(unix)]
pub fn plan_clone_dir(state_dir: &Path, origin: &str) -> Result<PathBuf, String> {
    checkout_of(&agent_root(state_dir).join("plan"), origin)
}

/// The file in a clone's git directory a session holds a shared lock on
/// while it runs, and `tidy` an exclusive one while it works on the clone.
#[cfg(unix)]
pub const SESSION_LOCK: &str = "zeughaus-session.lock";

/// What both sessions are started with.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct SessionArgs {
    repo: String,
    branch: Option<String>,
    proposal: Option<u64>,
    plan: Option<u64>,
    origin: Option<String>,
}

/// `<repo> [--branch <branch>] [--proposal <n>] [--plan <n>] [--origin
/// <origin>]`, the arguments both sessions take.
#[cfg(unix)]
fn parse_args(args: &[String]) -> Result<SessionArgs, String> {
    let mut repo = None;
    let mut branch = None;
    let mut proposal = None;
    let mut plan = None;
    let mut origin = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--branch" => match rest.next() {
                Some(value) if !value.is_empty() => branch = Some(value.clone()),
                _ => return Err("--branch needs a branch".to_owned()),
            },
            flag @ ("--proposal" | "--plan") => {
                match rest.next().and_then(|value| value.parse::<u64>().ok()) {
                    Some(number) if number >= 1 && flag == "--proposal" => proposal = Some(number),
                    Some(number) if number >= 1 => plan = Some(number),
                    _ => return Err(format!("{flag} needs a document number")),
                }
            }
            "--origin" => match rest.next() {
                Some(value) if !value.is_empty() => origin = Some(value.clone()),
                _ => return Err("--origin needs an origin".to_owned()),
            },
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag:?}")),
            _ if repo.is_none() => repo = Some(arg.clone()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let repo = repo.ok_or_else(|| "a repository name is needed".to_owned())?;
    if !crate::names::valid_name(&repo) {
        return Err(format!("{repo:?} is no repository name"));
    }
    Ok(SessionArgs {
        repo,
        branch,
        proposal,
        plan,
        origin,
    })
}

/// How often the master looks again at a clone it cannot start in.
#[cfg(unix)]
const MASTER_RETRY: std::time::Duration = std::time::Duration::from_secs(15);

/// The omp the sessions run: `$ZEUGHAUS_OMP`, else `omp` on the PATH.
#[cfg(unix)]
fn omp_program() -> std::ffi::OsString {
    std::env::var_os("ZEUGHAUS_OMP")
        .filter(|program| !program.is_empty())
        .unwrap_or_else(|| "omp".into())
}

/// What the master tells omp to do with its plan; `machines` is whether
/// `ZEUGHAUS_MACHINES` names other hosts.
#[cfg(unix)]
fn instruction(repo: &str, number: u64, branch: &str, machines: bool) -> String {
    let mut text = format!(
        "Execute the attached approved zeughaus plan #{number} of {repo} from top to bottom. \
         When its work is complete (gate passed, committed and pushed as AGENTS.md describes), \
         call the zeughaus MCP tool plan_done with repo {repo} and number {number}. \
         Other plans of this repository may be executing on other runners at the same time: \
         fetch and rebase onto origin/{branch} right before every push."
    );
    if machines {
        text.push_str(
            " This session has the machines in ZEUGHAUS_MACHINES (name=ssh destination) for \
             work on other hosts.",
        );
    }
    text
}

/// What `plan` tells omp to do with an attached proposal.
#[cfg(unix)]
fn proposal_instruction(repo: &str, number: u64, source: &str) -> String {
    format!(
        "Make a plan for the attached zeughaus proposal #{number} of {repo}, written from \
         {source}. It is a request: check it against this repository before planning. Put the \
         line `Proposals: #{number}` directly under the plan's title. When the plan is complete, \
         propose it with the propose_plan tool; zeughaus then asks the user what to do with it."
    )
}

#[cfg(not(unix))]
pub fn plan(_args: &[String], _state_dir: &Path) -> ExitCode {
    eprintln!("[agent] not supported on this platform");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
pub fn master(_args: &[String], _state_dir: &Path) -> ExitCode {
    eprintln!("[agent] not supported on this platform");
    ExitCode::FAILURE
}

#[cfg(unix)]
pub use self::unix::{Agents, master, plan};

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode, Stdio};

    use zeughaus_link::enlist::SessionSpec;
    use zeughaus_link::{CiReply, CiRequest, DocKind, DocState, DocView, SessionKind};
    use zeughaus_mux::{Dimensions, TerminalId};
    use zeughaus_terminal::Profile;
    use zeughaus_terminal::wal::Wal;

    use super::{
        MASTER_RETRY, SESSION_LOCK, SessionArgs, agent_root, clone_url, instruction, master_dir,
        omp_program, parse_args, plan_clone_dir, proposal_instruction,
    };
    use crate::enlist::SessionHost;
    use crate::mcp::client::{Client, Link};
    use crate::mux::{MuxService, OwnedPlacement};

    /// Opens agent sessions in the runner's mux.
    pub struct Agents {
        mux: MuxService,
        state_dir: PathBuf,
        exe: PathBuf,
    }

    impl Agents {
        pub fn new(mux: MuxService, state_dir: PathBuf, exe: PathBuf) -> Agents {
            Agents {
                mux,
                state_dir,
                exe,
            }
        }
    }

    /// `ZEUGHAUS_MACHINES`: `name=ssh` for each machine with an ssh
    /// destination, `;`-separated.
    fn machines_env(spec: &SessionSpec) -> String {
        spec.machines
            .iter()
            .filter_map(|m| Some(format!("{}={}", m.name, m.ssh.as_deref()?)))
            .collect::<Vec<_>>()
            .join(";")
    }

    impl SessionHost for Agents {
        fn open(&self, spec: &SessionSpec) -> Result<u64, String> {
            if !crate::names::valid_name(&spec.repo) {
                return Err(format!("{:?} is no repository name", spec.repo));
            }
            let master = master_dir(&self.state_dir, &spec.origin)?;
            let dir = match spec.kind {
                SessionKind::Master => master.clone(),
                SessionKind::Plan => plan_clone_dir(&self.state_dir, &spec.origin)?,
            };
            for clone in [&master, &dir] {
                if clone.exists() && !clone.is_dir() {
                    return Err(format!("{} exists and is no checkout", clone.display()));
                }
            }
            let (word, label, doc_flag) = match (spec.kind, spec.doc) {
                (SessionKind::Plan, Some(n)) => {
                    ("plan", format!("plan {} #{n}", spec.repo), "--proposal")
                }
                (SessionKind::Plan, None) => ("plan", format!("plan {}", spec.repo), ""),
                (SessionKind::Master, Some(n)) => {
                    ("master", format!("master {} #{n}", spec.repo), "--plan")
                }
                (SessionKind::Master, None) => {
                    return Err("a master session needs its plan".to_owned());
                }
            };
            // A missing clone is made by the session itself, in the
            // terminal, so the terminal starts in the agent root.
            let cwd = if dir.is_dir() {
                dir
            } else {
                let root = agent_root(&self.state_dir);
                std::fs::create_dir_all(&root)
                    .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
                root
            };
            let mut args = vec![
                "--state-dir".to_owned(),
                self.state_dir.to_string_lossy().into_owned(),
                word.to_owned(),
                spec.repo.clone(),
                "--branch".to_owned(),
                spec.default_branch.clone(),
                "--origin".to_owned(),
                spec.origin.clone(),
            ];
            if let Some(n) = spec.doc {
                args.extend([doc_flag.to_owned(), n.to_string()]);
            }
            let mut env = vec![
                ("ZEUGHAUS_SESSION".to_owned(), spec.id.to_string()),
                ("ZEUGHAUS_RUNNER".to_owned(), spec.runner.clone()),
            ];
            if !spec.ci.is_empty() {
                env.push(("ZEUGHAUS_CI".to_owned(), spec.ci.clone()));
            }
            let machines = machines_env(spec);
            if !machines.is_empty() {
                env.push(("ZEUGHAUS_MACHINES".to_owned(), machines));
            }
            let profile = Profile {
                label,
                program: Some(self.exe.clone()),
                args,
                cwd: Some(cwd),
                env,
                scrollback_rows: 20_000,
            };
            // A plan session is someone's conversation: it opens as a tab. A
            // master works alone and is attached by whoever wants to look.
            let placement = match spec.kind {
                SessionKind::Plan => OwnedPlacement::Triggered,
                SessionKind::Master => OwnedPlacement::Detached,
            };
            let TerminalId(terminal) = self.mux.spawn_owned(
                profile,
                Wal::Capped,
                Dimensions { cols: 80, rows: 24 },
                None,
                placement,
                zeughaus_link::ActivityKind::Session,
            )?;
            Ok(terminal)
        }

        fn close(&self, terminal: u64) -> Result<(), String> {
            self.mux.close_terminal(TerminalId(terminal))
        }

        fn state(&self, terminal: u64) -> Option<Option<zeughaus_mux::ExitState>> {
            self.mux.terminal_state(TerminalId(terminal))
        }
    }

    /// Runs git in `dir`; `Err` is its last line of stderr. It never asks
    /// for credentials: nobody would answer.
    fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
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

    /// Clones `origin` into `dir` when it is missing and checks `branch`
    /// out: the branch itself in the master clone (no `reference`), or
    /// `origin/<branch>` detached in the plan clone, which borrows the
    /// objects of the `reference` clone.
    fn ensure_clone(
        word: &str,
        dir: &Path,
        origin: &str,
        branch: &str,
        reference: Option<&Path>,
    ) -> Result<(), String> {
        if dir.exists() {
            if !dir.is_dir() {
                return Err(format!("{} exists and is no checkout", dir.display()));
            }
            return Ok(());
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let url = clone_url(origin);
        println!("[{word}] cloning {url} into {}", dir.display());
        // Inherited stdio: the progress and an ssh prompt belong in the
        // session's terminal.
        let mut command = Command::new("git");
        command.arg("clone");
        if let Some(reference) = reference {
            command.arg("--reference").arg(reference);
        }
        let status = command
            .arg("--no-checkout")
            .arg(&url)
            .arg(dir)
            .status()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if !status.success() {
            return Err(format!("git clone {url} failed ({status})"));
        }
        let tracking = format!("origin/{branch}");
        match reference {
            None => git(dir, &["checkout", "-q", branch])?,
            Some(_) => git(dir, &["checkout", "-q", "--detach", &tracking])?,
        };
        Ok(())
    }

    /// A shared lock on the clone's session lock file, held as long as the
    /// returned file lives. It only waits while `tidy` holds the file.
    fn session_lock(dir: &Path) -> Result<std::fs::File, String> {
        let path = dir.join(".git").join(SESSION_LOCK);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        file.lock_shared()
            .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        Ok(file)
    }

    /// `zeughaus-runner plan <repo> --branch <b> --origin <o> [--proposal
    /// <n>]`: brings the plan clone to the default branch and runs omp in
    /// plan mode there, planning proposal `n` of the CI when one is named.
    /// It holds the clone's session lock until omp ends and exits with
    /// omp's code.
    pub fn plan(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[plan] {message}");
            ExitCode::FAILURE
        };
        let (repo, branch, proposal, origin) = match parse_args(args) {
            Ok(SessionArgs { plan: Some(_), .. }) => {
                return fail("--plan is for master only".to_owned());
            }
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                proposal,
                origin: Some(origin),
                ..
            }) => (repo, branch, proposal, origin),
            Ok(SessionArgs { branch: None, .. }) => return fail("--branch is needed".to_owned()),
            Ok(SessionArgs { origin: None, .. }) => return fail("--origin is needed".to_owned()),
            Err(e) => return fail(e),
        };
        let proposal = match proposal {
            Some(n) => match fetch_proposal(&repo, n, state_dir) {
                Ok(doc) => Some(doc),
                Err(e) => return fail(e),
            },
            None => None,
        };
        let (master, dir) = match (
            master_dir(state_dir, &origin),
            plan_clone_dir(state_dir, &origin),
        ) {
            (Ok(master), Ok(dir)) => (master, dir),
            (Err(e), _) | (_, Err(e)) => return fail(e),
        };
        if let Err(e) = ensure_clone("plan", &master, &origin, &branch, None) {
            return fail(e);
        }
        let tracking = format!("origin/{branch}");
        match git(&master, &["fetch", "-q", "origin", &branch]) {
            Ok(_) => println!("[plan] fetched {tracking}"),
            Err(e) => println!("[plan] fetch failed: {e}; using the last fetched {tracking}"),
        }
        let mut cloned_again = false;
        loop {
            if let Err(e) = ensure_clone("plan", &dir, &origin, &branch, Some(&master)) {
                return fail(e);
            }
            match git(&dir, &["status", "--porcelain"]) {
                Ok(status) if status.trim().is_empty() => {}
                Ok(_) => {
                    println!(
                        "[plan] {} has changes; planning on it as it is",
                        dir.display()
                    );
                    break;
                }
                Err(e) => return fail(e),
            }
            let brought = git(&dir, &["fetch", "-q", "origin", &branch])
                .and_then(|_| git(&dir, &["checkout", "-q", "--detach", &tracking]));
            match brought {
                Ok(_) => {
                    println!("[plan] {} at {tracking}", dir.display());
                    break;
                }
                // A clean clone holds nothing of value: make it anew once.
                Err(e) if !cloned_again => {
                    println!(
                        "[plan] {} was unusable ({e}); cloning it again",
                        dir.display()
                    );
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        return fail(format!("cannot remove {}: {e}", dir.display()));
                    }
                    cloned_again = true;
                }
                Err(e) => return fail(e),
            }
        }
        let _lock = match session_lock(&dir) {
            Ok(lock) => lock,
            Err(e) => return fail(e),
        };
        let mut command = Command::new(omp_program());
        command
            .arg("--model")
            .arg("@plan")
            .env("ZEUGHAUS_PLAN_SESSION", &repo)
            .env("ZEUGHAUS_STATE_DIR", state_dir)
            .current_dir(&dir);
        if let Some(doc) = &proposal {
            let file = state_dir
                .join("agents")
                .join(&repo)
                .join(format!("proposal-{}.md", doc.number));
            if let Err(e) = crate::files::write_atomic(&file, doc.body.as_bytes()) {
                return fail(e);
            }
            command
                .arg(format!("@{}", file.display()))
                .arg(proposal_instruction(&repo, doc.number, &doc.source))
                .env("ZEUGHAUS_PROPOSAL", doc.number.to_string());
        }
        match command.status() {
            Ok(status) => status
                .code()
                .map_or(ExitCode::FAILURE, |code| ExitCode::from(code as u8)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fail("omp not found on PATH".to_owned())
            }
            Err(e) => fail(format!("cannot start omp: {e}")),
        }
    }

    /// Proposal `number` of `repo` from the CI, refused unless it is one.
    fn fetch_proposal(repo: &str, number: u64, state_dir: &Path) -> Result<DocView, String> {
        let doc = fetch_doc(repo, number, state_dir)?;
        if doc.kind != DocKind::Proposal {
            return Err(format!("#{number} of {repo} is a plan, not a proposal"));
        }
        if doc.state != DocState::Open {
            println!(
                "[plan] proposal #{number} is {}; planning it anyway",
                doc.state.name()
            );
        }
        Ok(doc)
    }

    /// Document `number` of `repo` from the CI, with its body.
    fn fetch_doc(repo: &str, number: u64, state_dir: &Path) -> Result<DocView, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the async runtime: {e}"))?;
        let client = Client::new(Link::resolve(None, state_dir));
        let asked = runtime.block_on(async {
            let asked = client
                .ci(CiRequest::Doc {
                    repo: repo.to_owned(),
                    number,
                })
                .await;
            client.shutdown().await;
            asked
        });
        match asked? {
            CiReply::Doc { doc: Some(doc) } => Ok(*doc),
            CiReply::Doc { doc: None } => Err(format!("no document #{number} in {repo}")),
            other => Err(crate::mcp::unexpected(&other)),
        }
    }

    /// `zeughaus-runner master <repo> --plan <n> --branch <b> --origin <o>`
    /// in the master clone: brings the clone to the default branch, waiting
    /// while it is dirty or ahead, and runs omp on plan `n`, which the CI put
    /// in progress on this runner. Exits with omp's code.
    pub fn master(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[master] {message}");
            ExitCode::FAILURE
        };
        let (repo, branch, origin, number) = match parse_args(args) {
            Ok(SessionArgs {
                proposal: Some(_), ..
            }) => return fail("--proposal is for plan only".to_owned()),
            Ok(SessionArgs { plan: None, .. }) => return fail("--plan is needed".to_owned()),
            Ok(SessionArgs { branch: None, .. }) => return fail("--branch is needed".to_owned()),
            Ok(SessionArgs { origin: None, .. }) => return fail("--origin is needed".to_owned()),
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                origin: Some(origin),
                plan: Some(number),
                ..
            }) => (repo, branch, origin, number),
            Err(e) => return fail(e),
        };
        let cwd = match master_dir(state_dir, &origin) {
            Ok(dir) => dir,
            Err(e) => return fail(e),
        };
        if let Err(e) = ensure_clone("master", &cwd, &origin, &branch, None) {
            return fail(e);
        }
        if let Err(e) = std::env::set_current_dir(&cwd) {
            return fail(format!("cannot enter {}: {e}", cwd.display()));
        }
        let _lock = match session_lock(&cwd) {
            Ok(lock) => lock,
            Err(e) => return fail(e),
        };
        let mut said: Option<String> = None;
        while let Err(reason) = bring_up_to_date(&cwd, &branch) {
            if said.as_deref() != Some(reason.as_str()) {
                println!("[master] {reason}; trying again every 15 s");
                said = Some(reason);
            }
            std::thread::sleep(MASTER_RETRY);
        }
        let plan = match fetch_doc(&repo, number, state_dir) {
            Ok(doc) => doc,
            Err(e) => return fail(e),
        };
        if plan.kind != DocKind::Plan || plan.state != DocState::InProgress {
            println!(
                "[master] plan #{number} is {}; nothing to do",
                plan.state.name()
            );
            return ExitCode::SUCCESS;
        }
        run_plan(&plan, &cwd, &branch, state_dir)
    }

    /// Puts the master clone on `branch` at `origin/<branch>`, so a claimed
    /// plan starts on the current default branch. `Err` is why it cannot,
    /// which the master waits out.
    fn bring_up_to_date(root: &Path, branch: &str) -> Result<(), String> {
        let status =
            git(root, &["status", "--porcelain"]).map_err(|e| format!("git status failed: {e}"))?;
        if !status.trim().is_empty() {
            return Err(format!("{} has uncommitted changes", root.display()));
        }
        git(root, &["fetch", "-q", "--prune", "origin", branch])
            .map_err(|e| format!("fetch failed: {e}"))?;
        let head = git(root, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
        if head.trim() != format!("refs/heads/{branch}") {
            git(root, &["checkout", "-q", branch])?;
        }
        let tracking = format!("origin/{branch}");
        let count = |range: String| -> Result<u64, String> {
            let counted = git(root, &["rev-list", "--count", &range])?;
            counted
                .trim()
                .parse()
                .map_err(|_| format!("git rev-list --count {range} answered {counted:?}"))
        };
        let ahead = count(format!("{tracking}..HEAD"))?;
        if ahead > 0 {
            return Err(format!(
                "{} has {ahead} unpushed commits on {branch}; push or reset them",
                root.display()
            ));
        }
        let behind = count(format!("HEAD..{tracking}"))?;
        if behind > 0 {
            git(root, &["merge", "--ff-only", "-q", &tracking])?;
            println!("[master] fast-forwarded {behind} commits");
        }
        Ok(())
    }

    /// Runs omp on `plan` in the checkout, with the terminal's stdio, and
    /// answers with omp's exit.
    fn run_plan(plan: &DocView, root: &Path, branch: &str, state_dir: &Path) -> ExitCode {
        let file = state_dir
            .join("agents")
            .join(&plan.repo)
            .join(format!("plan-{}.md", plan.number));
        if let Err(e) = crate::files::write_atomic(&file, plan.body.as_bytes()) {
            eprintln!("[master] {e}");
            return ExitCode::FAILURE;
        }
        println!("[master] plan #{} {}", plan.number, plan.title);
        let machines = std::env::var_os("ZEUGHAUS_MACHINES").is_some_and(|m| !m.is_empty());
        let status = Command::new(omp_program())
            .arg(format!("@{}", file.display()))
            .arg(instruction(&plan.repo, plan.number, branch, machines))
            .env("ZEUGHAUS_PLAN", format!("{}#{}", plan.repo, plan.number))
            .current_dir(root)
            .status();
        match status {
            Ok(status) => {
                println!("[master] omp ended ({status})");
                status
                    .code()
                    .map_or(ExitCode::FAILURE, |code| ExitCode::from(code as u8))
            }
            // The plan stays in progress; the user requeues it or marks it
            // done.
            Err(e) => {
                eprintln!("[master] cannot start omp: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn an_origin_names_a_checkout_three_levels_down() {
        let root = Path::new("/home/u/work");
        assert_eq!(
            checkout_of(root, "git.doodleshnookie.net/tuco86/zeughaus"),
            Ok(PathBuf::from(
                "/home/u/work/git.doodleshnookie.net/tuco86/zeughaus"
            ))
        );
        assert_eq!(
            checkout_of(root, "git.example.org:2222/o/r"),
            Ok(PathBuf::from("/home/u/work/git.example.org:2222/o/r"))
        );
        for bad in [
            "",
            "/etc/x/y",
            "host/repo",
            "host/../x/y",
            "host/./x",
            "host//x/y",
            "host/o/r/",
            "Host/o/r",
            "host/o/r x",
        ] {
            assert!(checkout_of(root, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn agent_clones_live_in_the_state_directory() {
        let state = Path::new("/s");
        assert_eq!(
            master_dir(state, "h/o/r"),
            Ok(PathBuf::from("/s/agent-work/master/h/o/r"))
        );
        assert_eq!(
            plan_clone_dir(state, "h/o/r"),
            Ok(PathBuf::from("/s/agent-work/plan/h/o/r"))
        );
        assert!(master_dir(state, "host/../x/y").is_err());
        assert!(plan_clone_dir(state, "host/../x/y").is_err());
    }

    #[test]
    fn a_checkout_is_cloned_over_ssh_from_its_origin() {
        assert_eq!(
            clone_url("git.doodleshnookie.net/tuco86/zeughaus"),
            "git@git.doodleshnookie.net:tuco86/zeughaus.git"
        );
        assert_eq!(
            clone_url("git.example.org:2222/o/r"),
            "ssh://git@git.example.org:2222/o/r.git"
        );
        assert_eq!(
            clone_url("github.com/griasdi/griasdi"),
            "git@github.com:griasdi/griasdi.git"
        );
    }

    #[test]
    fn both_sessions_take_a_repository_a_branch_a_document_and_an_origin() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let session = |branch: Option<&str>, proposal: Option<u64>| SessionArgs {
            repo: "demo".to_owned(),
            branch: branch.map(str::to_owned),
            proposal,
            plan: None,
            origin: None,
        };
        assert_eq!(
            parse_args(&args(&["demo", "--branch", "main"])),
            Ok(session(Some("main"), None))
        );
        assert_eq!(parse_args(&args(&["demo"])), Ok(session(None, None)));
        assert_eq!(
            parse_args(&args(&["demo", "--branch", "main", "--proposal", "4"])),
            Ok(session(Some("main"), Some(4)))
        );
        assert_eq!(
            parse_args(&args(&["demo", "--branch", "main", "--origin", "h/o/demo"])),
            Ok(SessionArgs {
                origin: Some("h/o/demo".to_owned()),
                ..session(Some("main"), None)
            })
        );
        assert_eq!(
            parse_args(&args(&["demo", "--plan", "29"])),
            Ok(SessionArgs {
                plan: Some(29),
                ..session(None, None)
            })
        );
        assert_eq!(
            parse_args(&args(&["demo", "--plan", "0"])),
            Err("--plan needs a document number".to_owned())
        );
        assert_eq!(
            parse_args(&args(&["demo", "--origin"])),
            Err("--origin needs an origin".to_owned())
        );
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["demo", "--branch"])).is_err());
        assert!(parse_args(&args(&["demo", "other"])).is_err());
        assert!(parse_args(&args(&["../x"])).is_err());
        for bad in [
            &["demo", "--proposal"][..],
            &["demo", "--proposal", "0"],
            &["demo", "--proposal", "x"],
        ] {
            assert_eq!(
                parse_args(&args(bad)),
                Err("--proposal needs a document number".to_owned()),
                "{bad:?}"
            );
        }
    }
}
