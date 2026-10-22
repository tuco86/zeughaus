//! Agent sessions an editor opens on its own runner, each in a terminal of
//! the runner's locked `Triggered` group (closing the tab ends it):
//!
//! - `plan <repo> [--proposal <n>]`: omp in plan mode in `<checkout>-plan`,
//!   a detached worktree of the repository's default branch that this keeps
//!   clean and current, so planning never touches the main checkout or its
//!   `target/`. With a proposal, omp starts planning that open proposal of
//!   the CI at once.
//! - `master <repo>`: the loop that claims the repository's approved plans
//!   from the CI one after another and executes each with omp in the main
//!   checkout. One session works in a checkout at a time: the CI refuses a
//!   claim while a plan is in progress, and a dirty checkout is waited for.
//!
//! A checkout is found by the repository's origin under the work root,
//! `~/work/<host>/<owner>/<repo>`.

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

/// The worktree plans are made in: `<checkout>-plan` beside the checkout.
#[cfg(unix)]
pub fn plan_dir(checkout: &Path) -> PathBuf {
    let mut name = checkout
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push("-plan");
    checkout.with_file_name(name)
}

/// What both sessions are started with.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct SessionArgs {
    repo: String,
    branch: Option<String>,
    proposal: Option<u64>,
}

/// `<repo> [--branch <branch>] [--proposal <n>]`, the arguments both
/// sessions take.
#[cfg(unix)]
fn parse_args(args: &[String]) -> Result<SessionArgs, String> {
    let mut repo = None;
    let mut branch = None;
    let mut proposal = None;
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
    })
}

/// The plan-mode overlay `plan` starts omp with.
#[cfg(unix)]
const PLAN_MODE: &str = "plan:\n  defaultOnStartup: true\n";

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
         propose it as plan mode describes; zeughaus then asks the user what to do with it."
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
    use std::os::unix::process::CommandExt;
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
        MASTER_POLL, PLAN_MODE, SessionArgs, checkout_of, instruction, parse_args, plan_dir,
        proposal_instruction, work_root,
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
            let Some(root) = work_root() else {
                return refused("no work root: neither ZEUGHAUS_WORK nor HOME is set".to_owned());
            };
            let checkout = match checkout_of(&root, &request.origin) {
                Ok(checkout) => checkout,
                Err(e) => return refused(e),
            };
            if !checkout.is_dir() {
                return refused(format!("no checkout at {}", checkout.display()));
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
            let mut args = vec![
                "--state-dir".to_owned(),
                self.state_dir.to_string_lossy().into_owned(),
                word.to_owned(),
                request.repo.clone(),
                "--branch".to_owned(),
                request.default_branch.clone(),
            ];
            if let Some(n) = request.proposal {
                args.extend(["--proposal".to_owned(), n.to_string()]);
            }
            let profile = Profile {
                label,
                program: Some(self.exe.clone()),
                args,
                cwd: Some(checkout),
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

    /// Runs git in `dir`; `Err` is its last line of stderr.
    fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
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

    /// `zeughaus-runner plan <repo> --branch <b> [--proposal <n>]` in the
    /// main checkout: brings `<checkout>-plan` to the default branch and
    /// replaces itself with omp in plan mode there, planning proposal `n`
    /// of the CI when one is named.
    pub fn plan(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[plan] {message}");
            ExitCode::FAILURE
        };
        let (repo, branch, proposal) = match parse_args(args) {
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                proposal,
            }) => (repo, branch, proposal),
            Ok(SessionArgs { branch: None, .. }) => return fail("--branch is needed".to_owned()),
            Err(e) => return fail(e),
        };
        let proposal = match proposal {
            Some(n) => match fetch_proposal(&repo, n, state_dir) {
                Ok(doc) => Some(doc),
                Err(e) => return fail(e),
            },
            None => None,
        };
        let checkout = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => return fail(format!("cannot read the working directory: {e}")),
        };
        let tracking = format!("origin/{branch}");
        match git(&checkout, &["fetch", "-q", "origin", &branch]) {
            Ok(_) => println!("[plan] fetched {tracking}"),
            Err(e) => println!("[plan] fetch failed: {e}; using the last fetched {tracking}"),
        }
        let dir = plan_dir(&checkout);
        let dir_arg = dir.to_string_lossy().into_owned();
        if !dir.exists() {
            if let Err(e) = git(
                &checkout,
                &["worktree", "add", "--detach", &dir_arg, &tracking],
            ) {
                return fail(e);
            }
            println!("[plan] made {dir_arg} at {tracking}");
        } else {
            match git(&dir, &["status", "--porcelain"]) {
                Ok(status) if status.trim().is_empty() => {
                    if let Err(e) = git(&dir, &["checkout", "-q", "--detach", &tracking]) {
                        return fail(e);
                    }
                    println!("[plan] {dir_arg} at {tracking}");
                }
                Ok(_) => println!("[plan] {dir_arg} has changes; planning on it as it is"),
                Err(e) => return fail(e),
            }
        }
        let overlay = state_dir.join("agents").join("plan-mode.yml");
        if let Err(e) = crate::files::write_atomic(&overlay, PLAN_MODE.as_bytes()) {
            return fail(e);
        }
        let mut command = Command::new("omp");
        command
            .arg("--config")
            .arg(&overlay)
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
        let error = command.exec();
        if error.kind() == std::io::ErrorKind::NotFound {
            fail("omp not found on PATH".to_owned())
        } else {
            fail(format!("cannot start omp: {error}"))
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

    /// `zeughaus-runner master <repo>` in the main checkout: claims the
    /// approved plans of `repo` one after another and runs omp on each.
    pub fn master(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[master] {message}");
            ExitCode::FAILURE
        };
        let repo = match parse_args(args) {
            Ok(SessionArgs {
                proposal: Some(_), ..
            }) => return fail("--proposal is for plan only".to_owned()),
            Ok(SessionArgs { repo, .. }) => repo,
            Err(e) => return fail(e),
        };
        let cwd = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => return fail(format!("cannot read the working directory: {e}")),
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
            let ended = serve_plans(&client, &repo, &cwd, state_dir).await;
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
            match git(&checkout.root, &["status", "--porcelain"]) {
                Ok(status) if status.trim().is_empty() => {}
                Ok(_) => {
                    waiting.say("checkout has uncommitted changes".to_owned());
                    tokio::time::sleep(MASTER_POLL).await;
                    continue;
                }
                Err(e) => {
                    waiting.say(format!("git status failed: {e}"));
                    tokio::time::sleep(MASTER_POLL).await;
                    continue;
                }
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
    fn plans_are_made_beside_the_checkout() {
        assert_eq!(
            plan_dir(Path::new("/w/h/o/zeughaus")),
            PathBuf::from("/w/h/o/zeughaus-plan")
        );
    }

    #[test]
    fn both_sessions_take_a_repository_a_branch_and_a_proposal() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let session = |branch: Option<&str>, proposal: Option<u64>| SessionArgs {
            repo: "demo".to_owned(),
            branch: branch.map(str::to_owned),
            proposal,
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
