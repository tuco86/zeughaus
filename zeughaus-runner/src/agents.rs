//! Agent sessions an editor opens on its own runner, each in a terminal of
//! the runner's locked `Triggered` group (closing the tab ends it):
//!
//! - `plan <repo> [--proposal <n>]`: omp in plan mode in the plan clone, a
//!   checkout detached at the repository's default branch that this keeps
//!   clean and current. It borrows the master clone's objects and never
//!   builds. With a proposal, omp starts planning that open proposal of the
//!   CI at once.
//! - `master <repo>`: the loop that claims the repository's approved plans
//!   from the CI one after another and executes each with omp in the master
//!   clone, which it fast-forwards to the default branch before each claim.
//!   One session works in a clone at a time: the CI refuses a claim while a
//!   plan is in progress, and a dirty or unpushed clone is waited for.
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
    origin: Option<String>,
}

/// `<repo> [--branch <branch>] [--proposal <n>] [--origin <origin>]`, the
/// arguments both sessions take.
#[cfg(unix)]
fn parse_args(args: &[String]) -> Result<SessionArgs, String> {
    let mut repo = None;
    let mut branch = None;
    let mut proposal = None;
    let mut origin = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--branch" => match rest.next() {
                Some(value) if !value.is_empty() => branch = Some(value.clone()),
                _ => return Err("--branch needs a branch".to_owned()),
            },
            "--proposal" => match rest.next().and_then(|value| value.parse::<u64>().ok()) {
                Some(number) if number >= 1 => proposal = Some(number),
                _ => return Err("--proposal needs a document number".to_owned()),
            },
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
        origin,
    })
}

/// How often the master looks for work.
#[cfg(unix)]
const MASTER_POLL: std::time::Duration = std::time::Duration::from_secs(15);

/// What the master tells omp to do with a claimed plan.
#[cfg(unix)]
fn instruction(repo: &str, number: u64) -> String {
    format!(
        "Execute the attached approved zeughaus plan #{number} of {repo} from top to bottom. \
         When its work is complete (gate passed, committed and pushed as AGENTS.md describes), \
         call the zeughaus MCP tool plan_done with repo {repo} and number {number}."
    )
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
pub use self::unix::{Agents, master, plan, serve};

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode, Stdio};
    use std::sync::Arc;

    use weida::{Replier, TransferMeta};
    use zeughaus_link::{
        AgentReply, AgentRequest, AgentSession, CiReply, CiRequest, DocKind, DocState, DocView,
        MAX_AGENT_BYTES,
    };
    use zeughaus_mux::{Dimensions, TerminalId};
    use zeughaus_terminal::Profile;
    use zeughaus_terminal::wal::Wal;

    use super::{
        MASTER_POLL, SESSION_LOCK, SessionArgs, agent_root, clone_url, instruction, master_dir,
        parse_args, plan_clone_dir, proposal_instruction,
    };
    use crate::mcp::client::{Client, Link};
    use crate::mcp::repo;
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

        fn open(&self, request: &AgentRequest) -> AgentReply {
            let refused = |message: String| AgentReply::Refused { message };
            if !crate::names::valid_name(&request.repo) {
                return refused(format!("{:?} is no repository name", request.repo));
            }
            let master = match master_dir(&self.state_dir, &request.origin) {
                Ok(dir) => dir,
                Err(e) => return refused(e),
            };
            let dir = match request.session {
                AgentSession::Master => master.clone(),
                AgentSession::Plan => match plan_clone_dir(&self.state_dir, &request.origin) {
                    Ok(dir) => dir,
                    Err(e) => return refused(e),
                },
            };
            for clone in [&master, &dir] {
                if clone.exists() && !clone.is_dir() {
                    return refused(format!("{} exists and is no checkout", clone.display()));
                }
            }
            let (word, label) = match (request.session, request.proposal) {
                (AgentSession::Plan, Some(n)) => ("plan", format!("plan {} #{n}", request.repo)),
                (AgentSession::Plan, None) => ("plan", format!("plan {}", request.repo)),
                (AgentSession::Master, Some(_)) => {
                    return refused("a master session takes no proposal".to_owned());
                }
                (AgentSession::Master, None) => ("master", format!("master {}", request.repo)),
            };
            if let Some(TerminalId(terminal)) = self.mux.find_labeled(&label) {
                return AgentReply::Opened {
                    terminal,
                    existing: true,
                };
            }
            // A missing clone is made by the session itself, in the
            // terminal, so the terminal starts in the agent root.
            let cwd = if dir.is_dir() {
                dir
            } else {
                let root = agent_root(&self.state_dir);
                if let Err(e) = std::fs::create_dir_all(&root) {
                    return refused(format!("cannot create {}: {e}", root.display()));
                }
                root
            };
            let mut args = vec![
                "--state-dir".to_owned(),
                self.state_dir.to_string_lossy().into_owned(),
                word.to_owned(),
                request.repo.clone(),
                "--branch".to_owned(),
                request.default_branch.clone(),
                "--origin".to_owned(),
                request.origin.clone(),
            ];
            if let Some(n) = request.proposal {
                args.extend(["--proposal".to_owned(), n.to_string()]);
            }
            let profile = Profile {
                label,
                program: Some(self.exe.clone()),
                args,
                cwd: Some(cwd),
                env: Vec::new(),
                scrollback_rows: 20_000,
            };
            match self.mux.spawn_owned(
                profile,
                Wal::Capped,
                Dimensions { cols: 80, rows: 24 },
                None,
                OwnedPlacement::Triggered,
            ) {
                Ok(TerminalId(terminal)) => AgentReply::Opened {
                    terminal,
                    existing: false,
                },
                Err(e) => refused(e),
            }
        }
    }

    /// Answers `/agent` requests.
    pub async fn serve(replier: Replier, agents: Arc<Agents>) {
        loop {
            let mut request = match replier.accept().await {
                Ok(request) => request,
                Err(e) => {
                    eprintln!("[runner] stopped serving agents: {e}");
                    return;
                }
            };
            let payload = match request.take_body().collect(MAX_AGENT_BYTES).await {
                Ok(payload) => payload,
                Err(e) => {
                    eprintln!("[runner] unreadable agent request: {e}");
                    continue;
                }
            };
            let Some(asked) = AgentRequest::decode(&payload) else {
                eprintln!(
                    "[runner] refused a malformed agent request ({} bytes)",
                    payload.len()
                );
                continue;
            };
            let opener = Arc::clone(&agents);
            let answer = match tokio::task::spawn_blocking(move || opener.open(&asked)).await {
                Ok(answer) => answer,
                Err(e) => AgentReply::Refused {
                    message: format!("the session could not be opened: {e}"),
                },
            };
            match &answer {
                AgentReply::Opened { terminal, existing } => eprintln!(
                    "[runner] agent session in terminal {terminal}{}",
                    if *existing { " (running)" } else { "" }
                ),
                AgentReply::Refused { message } => {
                    eprintln!("[runner] agent session refused: {message}")
                }
            }
            let mut reply = match request.reply(TransferMeta::default()).await {
                Ok(reply) => reply,
                Err(e) => {
                    eprintln!("[runner] cannot reply to an agent request: {e}");
                    continue;
                }
            };
            if let Err(e) = reply.write_all(&answer.encode()).await {
                eprintln!("[runner] cannot write an agent reply: {e}");
                continue;
            }
            if let Err(e) = reply.finish() {
                eprintln!("[runner] cannot finish an agent reply: {e}");
            }
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
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                proposal,
                origin: Some(origin),
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
        let mut command = Command::new("omp");
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
        let doc = match asked? {
            CiReply::Doc { doc: Some(doc) } => *doc,
            CiReply::Doc { doc: None } => return Err(format!("no proposal #{number} in {repo}")),
            other => return Err(crate::mcp::unexpected(&other)),
        };
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

    /// Prints a reason to wait, once while it stays the same.
    struct Waiting {
        last: Option<String>,
    }

    impl Waiting {
        fn say(&mut self, reason: String) {
            if self.last.as_deref() != Some(reason.as_str()) {
                println!("[master] {reason}");
                self.last = Some(reason);
            }
        }
    }

    /// `zeughaus-runner master <repo> --branch <b> --origin <o>` in the
    /// master clone: claims the approved plans of `repo` one after another
    /// and runs omp on each, holding the clone's session lock throughout.
    pub fn master(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[master] {message}");
            ExitCode::FAILURE
        };
        let (repo, branch, origin) = match parse_args(args) {
            Ok(SessionArgs {
                proposal: Some(_), ..
            }) => return fail("--proposal is for plan only".to_owned()),
            Ok(SessionArgs { branch: None, .. }) => return fail("--branch is needed".to_owned()),
            Ok(SessionArgs { origin: None, .. }) => return fail("--origin is needed".to_owned()),
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                origin: Some(origin),
                ..
            }) => (repo, branch, origin),
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
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => return fail(format!("cannot start the async runtime: {e}")),
        };
        let client = Client::new(Link::resolve(None, state_dir));
        let ended = runtime.block_on(async {
            let ended = serve_plans(&client, &repo, &branch, &cwd, state_dir).await;
            client.shutdown().await;
            ended
        });
        match ended {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        }
    }

    /// The master's loop; it only returns when the checkout is wrong.
    async fn serve_plans(
        client: &Client,
        repo: &str,
        branch: &str,
        cwd: &Path,
        state_dir: &Path,
    ) -> Result<(), String> {
        let checkout = repo::checkout(cwd)?;
        match repo::ask(client, &checkout, false).await? {
            Some(view) if view.name == repo => {}
            Some(view) => {
                return Err(format!(
                    "{} is {} in the CI, not {repo}",
                    checkout.root.display(),
                    view.name
                ));
            }
            None => {
                return Err(format!(
                    "{} is no repository of the CI's ci.toml",
                    checkout.origin
                ));
            }
        }
        println!("[master] {repo} in {}", checkout.root.display());
        let mut waiting = Waiting { last: None };
        loop {
            let (root, wanted) = (checkout.root.clone(), branch.to_owned());
            let ready = tokio::task::spawn_blocking(move || bring_up_to_date(&root, &wanted))
                .await
                .unwrap_or_else(|e| Err(format!("the checkout could not be updated: {e}")));
            if let Err(reason) = ready {
                waiting.say(reason);
                tokio::time::sleep(MASTER_POLL).await;
                continue;
            }
            let claimed = client
                .ci(CiRequest::PlanClaim {
                    repo: repo.to_owned(),
                })
                .await;
            let plan = match claimed {
                Ok(CiReply::Doc { doc: Some(plan) }) => *plan,
                Ok(CiReply::Doc { doc: None }) => {
                    waiting.say(format!("waiting for an approved plan of {repo}"));
                    tokio::time::sleep(MASTER_POLL).await;
                    continue;
                }
                Ok(other) => {
                    waiting.say(crate::mcp::unexpected(&other));
                    tokio::time::sleep(MASTER_POLL).await;
                    continue;
                }
                Err(e) => {
                    waiting.say(e);
                    tokio::time::sleep(MASTER_POLL).await;
                    continue;
                }
            };
            waiting.last = None;
            run_plan(&plan, &checkout.root, state_dir).await?;
            settle(client, repo, plan.number, &mut waiting).await;
        }
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

    /// Runs omp on `plan` in the checkout, with the terminal's stdio.
    async fn run_plan(plan: &DocView, root: &Path, state_dir: &Path) -> Result<(), String> {
        let file = state_dir
            .join("agents")
            .join(&plan.repo)
            .join(format!("plan-{}.md", plan.number));
        crate::files::write_atomic(&file, plan.body.as_bytes())?;
        println!("[master] plan #{} {}", plan.number, plan.title);
        let mut command = Command::new("omp");
        command
            .arg(format!("@{}", file.display()))
            .arg(instruction(&plan.repo, plan.number))
            .env("ZEUGHAUS_PLAN", format!("{}#{}", plan.repo, plan.number))
            .current_dir(root);
        let ran = tokio::task::spawn_blocking(move || command.status())
            .await
            .map_err(|e| format!("omp could not be waited for: {e}"))?;
        match ran {
            Ok(status) => println!("[master] omp ended ({status})"),
            // The plan stays in progress; the user requeues it or marks it
            // done, and the master waits for that below.
            Err(e) => println!("[master] cannot start omp: {e}"),
        }
        Ok(())
    }

    /// After omp ended: done goes on at once; anything else waits until
    /// the plan is no longer in progress.
    async fn settle(client: &Client, repo: &str, number: u64, waiting: &mut Waiting) {
        loop {
            let asked = client
                .ci(CiRequest::Doc {
                    repo: repo.to_owned(),
                    number,
                })
                .await;
            match asked {
                Ok(CiReply::Doc { doc: Some(doc) }) if doc.state == DocState::Done => {
                    println!("[master] plan #{number} done");
                    return;
                }
                Ok(CiReply::Doc { doc: Some(doc) }) if doc.state == DocState::InProgress => {
                    waiting.say(format!(
                        "plan #{number} is in_progress after omp ended; Requeue or Mark done in \
                         the desktop"
                    ));
                }
                Ok(CiReply::Doc { doc: Some(doc) }) => {
                    println!("[master] plan #{number} is {}", doc.state.name());
                    return;
                }
                Ok(CiReply::Doc { doc: None }) => {
                    println!("[master] plan #{number} is gone");
                    return;
                }
                Ok(other) => waiting.say(crate::mcp::unexpected(&other)),
                Err(e) => waiting.say(e),
            }
            tokio::time::sleep(MASTER_POLL).await;
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
    fn both_sessions_take_a_repository_a_branch_a_proposal_and_an_origin() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let session = |branch: Option<&str>, proposal: Option<u64>| SessionArgs {
            repo: "demo".to_owned(),
            branch: branch.map(str::to_owned),
            proposal,
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
