//! Agent sessions the CI opens on this runner, each in a terminal of the
//! runner's mux:
//!
//! - `plan <repo> [--ticket <n> [--resume <file>]]`: omp in plan mode in
//!   the plan clone, a checkout detached at the repository's default
//!   branch that this keeps clean and current. It borrows the master
//!   clone's objects and never builds. With a ticket, omp starts planning
//!   that ticket of the CI at once; after a Refine it resumes the omp
//!   session that wrote the ticket's plan with the user's review attached.
//! - `master <repo> --ticket <n>`: omp executing the plan of ticket `n`,
//!   which the CI put in progress on this runner, in the master clone. A
//!   clean clone is fast-forwarded to the default branch first; the
//!   ticket's earlier omp session is resumed whenever there is one, and
//!   after every exit of omp while the ticket is still in progress. A clone
//!   that holds work of another ticket, or work no master recorded, refuses
//!   the session.
//!
//! Both clones live in the runner's state directory, never among the user's
//! own checkouts: `<state-dir>/agent-work/{master,plan}/<host>/<owner>/<repo>`.
//! A missing one is cloned over ssh from the origin in the session's terminal
//! before the session starts. While a session runs it holds a shared lock on
//! the clone's `.git/zeughaus-session.lock`, so `tidy` leaves the clone alone.

use std::path::Path;
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
pub fn checkout_of(root: &Path, origin: &str) -> Result<PathBuf, String> {
    let refused = || format!("{origin:?} names no checkout under the work root");
    if origin.is_empty()
        || origin.starts_with('/')
        || !origin.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'.' | b'_' | b':' | b'/' | b'-')
        })
        || (cfg!(windows) && origin.contains(':'))
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
fn clone_url(origin: &str) -> String {
    let (host, rest) = origin.split_once('/').unwrap_or((origin, ""));
    if host.contains(':') {
        format!("ssh://git@{host}/{rest}.git")
    } else {
        format!("git@{host}:{rest}.git")
    }
}

/// The directory the agent sessions' clones live under.
pub fn agent_root(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-work")
}

/// The clone the master session executes the plans of `origin` in.
pub fn master_dir(state_dir: &Path, origin: &str) -> Result<PathBuf, String> {
    checkout_of(&agent_root(state_dir).join("master"), origin)
}

/// The clone plans of `origin` are made in; it borrows the master clone's
/// objects.
pub fn plan_clone_dir(state_dir: &Path, origin: &str) -> Result<PathBuf, String> {
    checkout_of(&agent_root(state_dir).join("plan"), origin)
}

/// The file in a clone's git directory a session holds a shared lock on
/// while it runs, and `tidy` an exclusive one while it works on the clone.
pub const SESSION_LOCK: &str = "zeughaus-session.lock";

/// The file in the master clone's `.git/` that names the ticket whose
/// master last started omp there. Its content is the ticket number and a
/// newline.
pub const MASTER_RECORD: &str = "zeughaus-master";

/// An omp that ends within `QUICK_OMP` of its start `MAX_QUICK` times in a
/// row ends the master.
const QUICK_OMP: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_QUICK: u32 = 3;

/// The omp session directory (`--session-dir`) of ticket `number`'s
/// masters.
fn master_sessions(state_dir: &Path, repo: &str, number: u64) -> PathBuf {
    state_dir
        .join("agents")
        .join(repo)
        .join(format!("master-{number}"))
}

/// What the master clone holds that `origin/<branch>` does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Holds {
    changed: usize,
    ahead: u64,
}

impl Holds {
    fn is_empty(self) -> bool {
        self.changed == 0 && self.ahead == 0
    }

    /// `3 changed files and 1 unpushed commit`, `1 changed file`, `2
    /// unpushed commits`, `no changes`.
    fn text(self) -> String {
        let plural =
            |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        if self.changed > 0 {
            parts.push(plural(self.changed as u64, "changed file", "changed files"));
        }
        if self.ahead > 0 {
            parts.push(plural(self.ahead, "unpushed commit", "unpushed commits"));
        }
        if parts.is_empty() {
            "no changes".to_owned()
        } else {
            parts.join(" and ")
        }
    }
}

/// Why a master cannot start in its clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The clone holds work no master recorded.
    Unrecorded,
    /// The clone holds work of another ticket.
    Other(u64),
    /// The clone holds work of this ticket, but its omp session is gone.
    SessionGone,
}

/// How a master starts omp in its clone.
#[derive(Debug, PartialEq, Eq)]
enum Start {
    Fresh,
    Resume(PathBuf),
    Refuse(Refusal),
}

/// How the master of `ticket` starts in a clone that holds `holds`, whose
/// record names `recorded`, with `earlier` the newest omp session of the
/// ticket's masters. A ticket with a session resumes it, also in a clean
/// clone; work in the clone is only ever resumed by its own ticket.
fn start_of(holds: Holds, recorded: Option<u64>, ticket: u64, earlier: Option<PathBuf>) -> Start {
    if holds.is_empty() {
        return earlier.map_or(Start::Fresh, Start::Resume);
    }
    match recorded {
        None => Start::Refuse(Refusal::Unrecorded),
        Some(m) if m != ticket => Start::Refuse(Refusal::Other(m)),
        Some(_) => earlier.map_or(Start::Refuse(Refusal::SessionGone), Start::Resume),
    }
}

/// Why the master cannot start in `root`, for the session's failure, the
/// ticket's note and the notice; `age` is the seconds since the newest
/// change in the clone.
fn refusal_text(
    root: &Path,
    sessions: &Path,
    refusal: Refusal,
    holds: Holds,
    age: Option<u64>,
) -> String {
    let held = format!("{} holds {}", root.display(), holds.text());
    let changed = age
        .map(|age| format!(", last changed {} ago", crate::mcp::docs::age(age)))
        .unwrap_or_default();
    match refusal {
        Refusal::Unrecorded => format!(
            "{held}, which no master recorded{changed}; commit and push, or stash them, to start"
        ),
        Refusal::Other(m) => format!(
            "{held} of ticket #{m}{changed}; Requeue #{m} to resume it there, or commit and \
             push, or stash them"
        ),
        Refusal::SessionGone => format!(
            "{held} of this ticket{changed}, but no omp session is left under {}; commit and \
             push, or stash them",
            sessions.display()
        ),
    }
}

/// Whether `value` is an omp session id rather than a session file: omp
/// finds a session by id prefix in its session directory.
pub fn is_session_id(value: &Path) -> bool {
    value.to_str().is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    })
}

/// What both sessions are started with.
#[derive(Debug, PartialEq, Eq)]
struct SessionArgs {
    repo: String,
    branch: Option<String>,
    ticket: Option<u64>,
    /// The omp session file a plan session resumes.
    resume: Option<PathBuf>,
    origin: Option<String>,
}

/// `<repo> [--branch <branch>] [--ticket <n>] [--resume <file>] [--origin
/// <origin>]`, the arguments both sessions take.
fn parse_args(args: &[String]) -> Result<SessionArgs, String> {
    let mut repo = None;
    let mut branch = None;
    let mut ticket = None;
    let mut resume = None;
    let mut origin = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--branch" => match rest.next() {
                Some(value) if !value.is_empty() => branch = Some(value.clone()),
                _ => return Err("--branch needs a branch".to_owned()),
            },
            "--ticket" => match rest.next().and_then(|value| value.parse::<u64>().ok()) {
                Some(number) if number >= 1 => ticket = Some(number),
                _ => return Err("--ticket needs a ticket number".to_owned()),
            },
            "--resume" => match rest.next() {
                Some(value) if !value.is_empty() => resume = Some(PathBuf::from(value)),
                _ => return Err("--resume needs a session file".to_owned()),
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
        ticket,
        resume,
        origin,
    })
}

/// The omp the sessions run: `$ZEUGHAUS_OMP`, else `omp` on the PATH.
fn omp_program() -> std::ffi::OsString {
    std::env::var_os("ZEUGHAUS_OMP")
        .filter(|program| !program.is_empty())
        .unwrap_or_else(|| "omp".into())
}

/// What the master tells omp to do with its ticket's plan; `machines` is
/// whether `ZEUGHAUS_MACHINES` names other hosts, `filed` the tickets the
/// plan's sections became, which the attached plan leaves out.
fn instruction(
    repo: &str,
    number: u64,
    branch: &str,
    machines: bool,
    filed: &[zeughaus_link::FiledTicket],
) -> String {
    let mut text = format!(
        "Execute the plan of the attached approved zeughaus ticket #{number} of {repo} from \
         top to bottom. When its work is complete (gate passed, committed and pushed as \
         AGENTS.md describes), write your final report (what was done, the verification run \
         with its figures, every target of the plan met or missed, anything left open or \
         deviating from the plan), call the zeughaus MCP tool plan_done with repo {repo}, \
         number {number} and that report as report, and end the session with the same text. \
         Other plans of this repository may be executing on other runners at the same time: \
         fetch and rebase onto origin/{branch} right before every push."
    );
    if machines {
        text.push_str(
            " This session has the machines in ZEUGHAUS_MACHINES (name=ssh destination) for \
             work on other hosts.",
        );
    }
    if !filed.is_empty() {
        let tickets: Vec<String> = filed
            .iter()
            .map(|ticket| {
                let reference = if ticket.repo == repo {
                    format!("#{}", ticket.number)
                } else {
                    format!("#{}-{}", ticket.repo, ticket.number)
                };
                format!("{reference} ({})", ticket.title)
            })
            .collect();
        text.push_str(&format!(
            " Its sections for other tickets were filed as tickets of their own and are left \
             out of the attached plan: {}.",
            tickets.join(", ")
        ));
    }
    text
}

/// What a master tells the omp session of its ticket it resumes, with
/// `holds` what the clone keeps of it.
fn resume_instruction(repo: &str, number: u64, branch: &str, holds: Holds) -> String {
    format!(
        "This omp session executes zeughaus ticket #{number} of {repo} and was interrupted; the \
         master clone keeps what it had ({}). Look at git status and git log \
         origin/{branch}..HEAD, then go on with the plan where it stopped. Fetch and rebase onto \
         origin/{branch} right before every push, and once its work is complete write your \
         final report (what was done, the verification run with its figures, every target of \
         the plan met or missed, anything left open or deviating from the plan), call the \
         zeughaus MCP tool plan_done with repo {repo}, number {number} and that report as \
         report, and end the session with the same text.",
        holds.text()
    )
}

/// What `plan` tells omp to do with an attached ticket.
fn ticket_instruction(repo: &str, number: u64, source: &str) -> String {
    format!(
        "Make a plan for the attached zeughaus ticket #{number} of {repo}, written by {source}. \
         It is a request: check it against this repository before planning. When the plan is \
         complete, submit it with the submit_plan tool: zeughaus stores it on the ticket for \
         the user's review, and this session ends."
    )
}

/// What a resumed plan session is told with the user's review attached.
fn refine_instruction(repo: &str, number: u64) -> String {
    format!(
        "The user reviewed your plan for zeughaus ticket #{number} of {repo}; the review is \
         attached. Revise the plan file: drop every section it deletes, act on every \
         annotation, then submit it again with submit_plan."
    )
}

/// What a plan session is told that revises a plan it cannot resume.
fn replan_instruction(repo: &str, number: u64) -> String {
    format!(
        "The attached plan for zeughaus ticket #{number} of {repo} was reviewed by the user; \
         the ticket, the plan and the review are attached. Write the revised plan to \
         local://<slug>-plan.md and submit it with submit_plan."
    )
}

/// The ticket a plan session is given: its title, what it asks for, its
/// links both ways, each with the linked ticket's title and state when
/// `docs` (its repository's tickets) has it, and its comments.
fn ticket_brief(doc: &zeughaus_link::DocView, docs: &[zeughaus_link::DocView], now: u64) -> String {
    use zeughaus_link::LinkKind;
    let mut text = format!("# #{} {}\n\n", doc.number, doc.title);
    let proposal = zeughaus_link::text_without_title(&doc.body);
    if proposal.trim().is_empty() {
        text.push_str("(no proposal text: this ticket started as a plan)\n");
    } else {
        text.push_str(proposal.trim_end());
        text.push('\n');
    }
    let line = |verb: &str, reference: String, same: Option<u64>| match same
        .and_then(|number| docs.iter().find(|d| d.number == number))
    {
        Some(other) => format!(
            "- {verb} {reference} {} ({})\n",
            other.title,
            other.state.name()
        ),
        None => format!("- {verb} {reference}\n"),
    };
    let mut links = String::new();
    for link in &doc.links {
        let verb = match link.kind {
            LinkKind::DependsOn => "depends on",
            LinkKind::Related => "related to",
        };
        let same = link.repo.is_none().then_some(link.number);
        links.push_str(&line(verb, link.reference(&doc.repo), same));
    }
    for (kind, number) in zeughaus_link::linked_from(doc.number, docs) {
        let verb = match kind {
            LinkKind::DependsOn => "blocks",
            LinkKind::Related => "related to",
        };
        links.push_str(&line(verb, format!("#{number}"), Some(number)));
    }
    if !links.is_empty() {
        text.push_str("\n## Links\n\n");
        text.push_str(&links);
    }
    if !doc.comments.is_empty() {
        text.push('\n');
        text.push_str(&crate::mcp::docs::comments_text(&doc.comments, now));
    }
    text
}

/// What a resumed Refine is given: the review, then the ticket's comments.
fn refine_text(doc: &zeughaus_link::DocView, now: u64) -> String {
    let mut text = doc.feedback.clone();
    if !doc.comments.is_empty() {
        text.push_str("\n\n");
        text.push_str(&crate::mcp::docs::comments_text(&doc.comments, now));
    }
    text
}

/// The text of the last assistant message that has any in an omp session
/// file (JSON lines), its text parts joined by a blank line; `None`
/// without one. Lines that do not parse are skipped.
fn last_assistant_text(jsonl: &str) -> Option<String> {
    jsonl.lines().rev().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        if entry["type"] != "message" || entry["message"]["role"] != "assistant" {
            return None;
        }
        let texts: Vec<&str> = entry["message"]["content"]
            .as_array()?
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .filter(|text| !text.trim().is_empty())
            .collect();
        (!texts.is_empty()).then(|| texts.join("\n\n"))
    })
}

pub use self::host::{Agents, master, plan, transcript_report};
#[cfg(unix)]
pub use self::host::{work, work_profile};

mod host {
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode, Stdio};

    use zeughaus_link::enlist::SessionSpec;
    use zeughaus_link::{CiReply, CiRequest, DocKind, DocState, DocView, SessionKind};
    use zeughaus_mux::{Dimensions, TerminalId};
    use zeughaus_terminal::Profile;
    use zeughaus_terminal::wal::Wal;

    use super::{
        Holds, MASTER_RECORD, MAX_QUICK, QUICK_OMP, SESSION_LOCK, SessionArgs, Start, agent_root,
        clone_url, instruction, last_assistant_text, master_dir, master_sessions, omp_program,
        parse_args, plan_clone_dir, refine_instruction, refine_text, refusal_text,
        replan_instruction, resume_instruction, start_of, ticket_brief, ticket_instruction,
    };
    #[cfg(unix)]
    use super::{checkout_of, parse_work_args, work_root};
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
            let (word, label) = match (spec.kind, spec.doc) {
                (SessionKind::Plan, Some(n)) => ("plan", format!("plan {} #{n}", spec.repo)),
                (SessionKind::Plan, None) => ("plan", format!("plan {}", spec.repo)),
                (SessionKind::Master, Some(n)) => ("master", format!("master {} #{n}", spec.repo)),
                (SessionKind::Master, None) => {
                    return Err("a master session needs its ticket".to_owned());
                }
            };
            // A master that cannot use its clone is refused here, before a
            // terminal opens, so the CI sees why at once. A missing clone is
            // cloned by the session, which then starts fresh.
            if let (SessionKind::Master, Some(n)) = (spec.kind, spec.doc)
                && master.is_dir()
            {
                let sessions = master_sessions(&self.state_dir, &spec.repo, n);
                master_start(&master, &spec.default_branch, n, &sessions)?;
            }
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
                args.extend(["--ticket".to_owned(), n.to_string()]);
            }
            if let Some(file) = &spec.resume {
                args.extend(["--resume".to_owned(), file.clone()]);
            }
            let mut env = vec![("ZEUGHAUS_SESSION".to_owned(), spec.id.to_string())];
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
            // Plan sessions and masters open as tabs in their repository's
            // locked group, where they are seen working; closing the tab
            // stops the session.
            let placement = OwnedPlacement::Project(spec.origin.clone());
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

        fn keeps_terminals(&self) -> bool {
            self.mux.keeps_terminals()
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

    /// On Windows nothing ends a process's children with it: closing the
    /// session's tab ends this process, and the job then ends omp and
    /// everything it started.
    #[cfg(windows)]
    fn own_tree(word: &str) -> Option<crate::winjob::Job> {
        match crate::winjob::Job::kill_on_close().and_then(|job| job.assign_self().map(|()| job)) {
            Ok(job) => Some(job),
            Err(e) => {
                eprintln!("[{word}] omp will outlive a closed tab: {e}");
                None
            }
        }
    }

    /// `zeughaus-runner plan <repo> --branch <b> --origin <o> [--ticket <n>
    /// [--resume <file>]]`: brings the plan clone to the default branch and
    /// runs omp in plan mode there, planning ticket `n` of the CI when one is
    /// named, and resuming the omp session `file` with the user's review
    /// after a Refine. It holds the clone's session lock until omp ends and
    /// exits with omp's code.
    pub fn plan(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[plan] {message}");
            ExitCode::FAILURE
        };
        let (repo, branch, ticket, resume, origin) = match parse_args(args) {
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                ticket,
                resume,
                origin: Some(origin),
            }) => (repo, branch, ticket, resume, origin),
            Ok(SessionArgs { branch: None, .. }) => return fail("--branch is needed".to_owned()),
            Ok(SessionArgs { origin: None, .. }) => return fail("--origin is needed".to_owned()),
            Err(e) => return fail(e),
        };
        #[cfg(windows)]
        let _tree = own_tree("plan");
        let ticket = match ticket {
            Some(n) => match fetch_ticket(&repo, n, state_dir) {
                Ok(ticket) => Some(ticket),
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
        if let Some((doc, docs)) = &ticket {
            let n = doc.number;
            let agents = state_dir.join("agents").join(&repo);
            let write = |name: String, text: &str| {
                let file = agents.join(name);
                crate::files::write_atomic(&file, text.as_bytes()).map(|()| file)
            };
            let at = |file: &Path| format!("@{}", file.display());
            let resumable = resume.filter(|file| {
                let there = file.is_file() || super::is_session_id(file);
                if !there {
                    println!(
                        "[plan] {} is gone; planning anew with the plan and its review attached",
                        file.display()
                    );
                }
                there
            });
            let now = crate::enlist::now_secs();
            let written = match resumable {
                Some(session) => {
                    write(format!("refine-{n}.md"), &refine_text(doc, now)).map(|refine| {
                        command
                            .arg("--resume")
                            .arg(session)
                            .arg(at(&refine))
                            .arg(refine_instruction(&repo, n));
                    })
                }
                None if !doc.feedback.is_empty() => {
                    write(format!("ticket-{n}.md"), &ticket_brief(doc, docs, now)).and_then(
                        |brief| {
                            let plan = write(format!("plan-{n}.md"), &doc.plan_text)?;
                            // The brief carries the comments here.
                            let refine = write(format!("refine-{n}.md"), &doc.feedback)?;
                            command
                                .arg(at(&brief))
                                .arg(at(&plan))
                                .arg(at(&refine))
                                .arg(replan_instruction(&repo, n));
                            Ok(())
                        },
                    )
                }
                None => {
                    write(format!("ticket-{n}.md"), &ticket_brief(doc, docs, now)).map(|brief| {
                        command
                            .arg(at(&brief))
                            .arg(ticket_instruction(&repo, n, &doc.source));
                    })
                }
            };
            if let Err(e) = written {
                return fail(e);
            }
            command.env("ZEUGHAUS_TICKET", n.to_string());
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

    /// Ticket `number` of `repo` from the CI with its texts, refused unless
    /// it is one, and the repository's tickets for the brief's links (none
    /// when the list cannot be had).
    fn fetch_ticket(
        repo: &str,
        number: u64,
        state_dir: &Path,
    ) -> Result<(DocView, Vec<DocView>), String> {
        let doc = fetch_doc(repo, number, state_dir)?;
        if doc.kind != DocKind::Ticket {
            return Err(format!("#{number} of {repo} is no ticket"));
        }
        if doc.state != DocState::Planning {
            println!(
                "[plan] ticket #{number} is {}; planning it anyway",
                doc.state.name()
            );
        }
        let docs = match ask_ci(
            CiRequest::Docs {
                repo: repo.to_owned(),
            },
            state_dir,
        ) {
            Ok(CiReply::Docs { docs }) => docs,
            _ => Vec::new(),
        };
        Ok((doc, docs))
    }

    /// Document `number` of `repo` from the CI, with its texts.
    fn fetch_doc(repo: &str, number: u64, state_dir: &Path) -> Result<DocView, String> {
        let request = CiRequest::Doc {
            repo: repo.to_owned(),
            number,
        };
        match ask_ci(request, state_dir)? {
            CiReply::Doc { doc: Some(doc) } => Ok(*doc),
            CiReply::Doc { doc: None } => Err(format!("no document #{number} in {repo}")),
            other => Err(crate::mcp::unexpected(&other)),
        }
    }

    /// One request to the CI this state directory dials.
    fn ask_ci(request: CiRequest, state_dir: &Path) -> Result<CiReply, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the async runtime: {e}"))?;
        let client = Client::new(Link::resolve(None, state_dir));
        runtime.block_on(async {
            let asked = client.ci(request).await;
            client.shutdown().await;
            asked
        })
    }

    /// `zeughaus-runner master <repo> --ticket <n> --branch <b> --origin
    /// <o>` in the master clone: runs omp on the plan of ticket `n`, which
    /// the CI put in progress on this runner, until the ticket is no longer
    /// in progress, resuming the ticket's omp session after each exit. It
    /// exits at once when the clone holds work it cannot resume.
    pub fn master(args: &[String], state_dir: &Path) -> ExitCode {
        let fail = |message: String| {
            eprintln!("[master] {message}");
            ExitCode::FAILURE
        };
        let (repo, branch, origin, number) = match parse_args(args) {
            Ok(SessionArgs {
                resume: Some(_), ..
            }) => return fail("--resume is for plan only".to_owned()),
            Ok(SessionArgs { ticket: None, .. }) => return fail("--ticket is needed".to_owned()),
            Ok(SessionArgs { branch: None, .. }) => return fail("--branch is needed".to_owned()),
            Ok(SessionArgs { origin: None, .. }) => return fail("--origin is needed".to_owned()),
            Ok(SessionArgs {
                repo,
                branch: Some(branch),
                origin: Some(origin),
                ticket: Some(number),
                ..
            }) => (repo, branch, origin, number),
            Err(e) => return fail(e),
        };
        #[cfg(windows)]
        let _tree = own_tree("master");
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
        let sessions = master_sessions(state_dir, &repo, number);
        let mut quick = 0;
        loop {
            let plan = match fetch_doc(&repo, number, state_dir) {
                Ok(doc) => doc,
                Err(e) => return fail(e),
            };
            if plan.kind != DocKind::Ticket || plan.state != DocState::InProgress {
                println!(
                    "[master] ticket #{number} is {}; nothing to do",
                    plan.state.name()
                );
                return ExitCode::SUCCESS;
            }
            let (start, holds) = match master_start(&cwd, &branch, number, &sessions) {
                Ok(start) => start,
                Err(reason) => return fail(reason),
            };
            if holds.is_empty()
                && let Err(e) = fast_forward(&cwd, &branch)
            {
                return fail(e);
            }
            if let Err(e) = record_ticket(&cwd, number) {
                return fail(e);
            }
            let launch = match &start {
                Start::Fresh => Launch::Fresh,
                Start::Resume(file) => Launch::Resume(file, holds),
                Start::Refuse(_) => unreachable!("master_start answers a refusal as Err"),
            };
            let started = std::time::Instant::now();
            let status = match run_omp(&plan, &cwd, &branch, state_dir, &sessions, launch) {
                Ok(status) => status,
                Err(e) => return fail(e),
            };
            println!("[master] omp ended ({status})");
            quick = if started.elapsed() < QUICK_OMP {
                quick + 1
            } else {
                0
            };
            if quick >= MAX_QUICK {
                return fail(format!(
                    "omp ended {MAX_QUICK} times within a minute of its start; the ticket stays \
                     in progress"
                ));
            }
        }
    }

    /// What the master clone holds that `origin/<branch>` does not, and the
    /// seconds since its newest changed file was modified.
    fn probe(root: &Path, branch: &str) -> Result<(Holds, Option<u64>), String> {
        let inspect = |e: String| format!("cannot inspect {}: {e}", root.display());
        let status = git(root, &["status", "--porcelain"]).map_err(inspect)?;
        let lines: Vec<&str> = status.lines().filter(|line| !line.is_empty()).collect();
        let now = std::time::SystemTime::now();
        let age = lines
            .iter()
            .filter_map(|line| line.get(3..))
            .map(|path| path.split_once(" -> ").map_or(path, |(_, to)| to))
            .filter(|path| !path.starts_with('"'))
            .filter_map(|path| {
                std::fs::symlink_metadata(root.join(path))
                    .ok()?
                    .modified()
                    .ok()
            })
            .max()
            .and_then(|newest| now.duration_since(newest).ok())
            .map(|age| age.as_secs());
        let range = format!("origin/{branch}..HEAD");
        let counted = git(root, &["rev-list", "--count", &range]).map_err(inspect)?;
        let ahead = counted
            .trim()
            .parse()
            .map_err(|_| inspect(format!("git rev-list --count {range} answered {counted:?}")))?;
        Ok((
            Holds {
                changed: lines.len(),
                ahead,
            },
            age,
        ))
    }

    /// The ticket whose master last started omp in the clone, if it says.
    fn recorded_ticket(root: &Path) -> Option<u64> {
        std::fs::read_to_string(root.join(".git").join(MASTER_RECORD))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Records ticket `number` as the one whose master runs omp in the clone.
    fn record_ticket(root: &Path, number: u64) -> Result<(), String> {
        crate::files::write_atomic(
            &root.join(".git").join(MASTER_RECORD),
            format!("{number}\n").as_bytes(),
        )
    }

    /// The newest non-empty omp session file directly in `dir`.
    fn earlier_session(dir: &Path) -> Option<PathBuf> {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                (meta.is_file() && meta.len() > 0).then_some((meta.modified().ok()?, entry.path()))
            })
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, path)| path)
    }

    /// What the master of ticket `number` of `repo` said last: from the
    /// newest omp session file of the ticket's masters, cut to
    /// `MAX_REPORT_BYTES`; `None` when there is none or it says nothing.
    pub fn transcript_report(state_dir: &Path, repo: &str, number: u64) -> Option<String> {
        let file = earlier_session(&master_sessions(state_dir, repo, number))?;
        let text = last_assistant_text(&std::fs::read_to_string(file).ok()?)?;
        let report = zeughaus_link::report_text(&text);
        (!report.is_empty()).then(|| report.to_owned())
    }

    /// How the master of `ticket` starts in `root`; `Ok` is `Fresh` or
    /// `Resume`, a refusal is `Err` with its reason.
    fn master_start(
        root: &Path,
        branch: &str,
        ticket: u64,
        sessions: &Path,
    ) -> Result<(Start, Holds), String> {
        let (holds, age) = probe(root, branch)?;
        match start_of(
            holds,
            recorded_ticket(root),
            ticket,
            earlier_session(sessions),
        ) {
            Start::Refuse(refusal) => Err(refusal_text(root, sessions, refusal, holds, age)),
            start => Ok((start, holds)),
        }
    }

    /// Puts a master clone that holds nothing on `branch` at
    /// `origin/<branch>`, so a plan starts on the current default branch.
    fn fast_forward(root: &Path, branch: &str) -> Result<(), String> {
        let tracking = format!("origin/{branch}");
        if let Err(e) = git(root, &["fetch", "-q", "--prune", "origin", branch]) {
            println!("[master] fetch failed: {e}; using the last fetched {tracking}");
        }
        let head = git(root, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
        if head.trim() != format!("refs/heads/{branch}") {
            git(root, &["checkout", "-q", branch])?;
        }
        let range = format!("HEAD..{tracking}");
        let counted = git(root, &["rev-list", "--count", &range])?;
        let behind: u64 = counted
            .trim()
            .parse()
            .map_err(|_| format!("git rev-list --count {range} answered {counted:?}"))?;
        if behind > 0 {
            git(root, &["merge", "--ff-only", "-q", &tracking])?;
            println!("[master] fast-forwarded {behind} commits");
        }
        Ok(())
    }

    /// How `run_omp` starts omp: on the plan, or resuming the ticket's
    /// session file in a clone that holds `Holds`.
    enum Launch<'a> {
        Fresh,
        Resume(&'a Path, Holds),
    }

    /// Runs omp on `plan` in the checkout with the terminal's stdio and its
    /// sessions in `sessions`, and answers with omp's exit.
    fn run_omp(
        plan: &DocView,
        root: &Path,
        branch: &str,
        state_dir: &Path,
        sessions: &Path,
        launch: Launch<'_>,
    ) -> Result<std::process::ExitStatus, String> {
        std::fs::create_dir_all(sessions)
            .map_err(|e| format!("cannot create {}: {e}", sessions.display()))?;
        let mut command = Command::new(omp_program());
        command.arg("--session-dir").arg(sessions);
        println!("[master] ticket #{} {}", plan.number, plan.title);
        match launch {
            Launch::Fresh => {
                let file = state_dir
                    .join("agents")
                    .join(&plan.repo)
                    .join(format!("plan-{}.md", plan.number));
                let text = zeughaus_link::without_sections(&plan.plan_text, &plan.filed);
                crate::files::write_atomic(&file, text.as_bytes())?;
                let machines = std::env::var_os("ZEUGHAUS_MACHINES").is_some_and(|m| !m.is_empty());
                command.arg(format!("@{}", file.display())).arg(instruction(
                    &plan.repo,
                    plan.number,
                    branch,
                    machines,
                    &plan.filed,
                ));
            }
            Launch::Resume(file, holds) => {
                println!("[master] resuming {}", file.display());
                command.arg("--resume").arg(file).arg(resume_instruction(
                    &plan.repo,
                    plan.number,
                    branch,
                    holds,
                ));
            }
        }
        command
            .env("ZEUGHAUS_PLAN", format!("{}#{}", plan.repo, plan.number))
            .current_dir(root)
            .status()
            .map_err(|e| format!("cannot start omp: {e}"))
    }

    /// The profile of a shell in the work checkout of `origin`: the
    /// runner's own `work` subcommand, which clones the checkout first when
    /// it is missing and then becomes the user's shell there.
    #[cfg(unix)]
    pub fn work_profile(
        state_dir: &Path,
        exe: &Path,
        origin: &str,
        branch: &str,
    ) -> Result<Profile, String> {
        let root = work_root().ok_or("no work root: HOME is not set")?;
        let dir = checkout_of(&root, origin)?;
        let name = origin.rsplit('/').next().unwrap_or(origin);
        let cwd = if dir.is_dir() {
            dir
        } else {
            std::fs::create_dir_all(&root)
                .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
            root
        };
        Ok(Profile {
            label: format!("~/work/{name}"),
            program: Some(exe.to_owned()),
            args: vec![
                "--state-dir".to_owned(),
                state_dir.to_string_lossy().into_owned(),
                "work".to_owned(),
                origin.to_owned(),
                "--branch".to_owned(),
                branch.to_owned(),
            ],
            cwd: Some(cwd),
            env: Vec::new(),
            scrollback_rows: 20_000,
        })
    }

    /// `zeughaus-runner work <host/owner/name> [--branch <branch>]`: clones
    /// the work checkout of the origin when it is missing, then replaces
    /// itself with the user's shell in it (in the work root when the clone
    /// failed).
    #[cfg(unix)]
    pub fn work(args: &[String], _state_dir: &Path) -> ExitCode {
        use std::os::unix::process::CommandExt;

        let Some((origin, branch)) = parse_work_args(args) else {
            eprintln!("usage: zeughaus-runner work <host/owner/name> [--branch <branch>]");
            return ExitCode::FAILURE;
        };
        let Some(root) = work_root() else {
            eprintln!("[work] no work root: HOME is not set");
            return ExitCode::FAILURE;
        };
        let dir = match checkout_of(&root, &origin)
            .and_then(|dir| ensure_clone("work", &dir, &origin, &branch, None).map(|()| dir))
        {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("[work] {e}");
                root
            }
        };
        let shell = std::env::var_os("SHELL")
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let e = Command::new(&shell).current_dir(&dir).exec();
        eprintln!("[work] cannot start {}: {e}", Path::new(&shell).display());
        ExitCode::FAILURE
    }
}

/// The arguments of `work`: one origin and an optional `--branch`, `main`
/// by default.
#[cfg(unix)]
fn parse_work_args(args: &[String]) -> Option<(String, String)> {
    let mut origin = None;
    let mut branch = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--branch" if branch.is_none() => branch = Some(args.next()?.clone()),
            flag if flag.starts_with('-') => return None,
            _ if origin.is_none() => origin = Some(arg.clone()),
            _ => return None,
        }
    }
    Some((origin?, branch.unwrap_or_else(|| "main".to_owned())))
}

#[cfg(not(unix))]
pub fn work(_args: &[String], _state_dir: &Path) -> ExitCode {
    eprintln!("[agent] not supported on this platform");
    ExitCode::FAILURE
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use zeughaus_link::{
        DocKind, DocLink, DocState, DocView, FiledTicket, LinkKind, TicketComment,
    };

    #[test]
    fn the_report_is_the_last_assistant_text() {
        let jsonl = [
            r#"{"type":"session","id":"s"}"#,
            r##"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"# Report\n\nAll done."}]}}"##,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"toolCall","name":"bash"}]}}"#,
            r#"{"type":"message","message":{"role":"toolResult","content":[{"type":"text","text":"ok"}]}}"#,
            "not json",
            r#"{"type":"custom","customType":"session_exit"}"#,
        ]
        .join("\n");
        assert_eq!(
            last_assistant_text(&jsonl).as_deref(),
            Some("# Report\n\nAll done.")
        );
        let user_only = r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#;
        assert_eq!(last_assistant_text(user_only), None);
    }

    #[test]
    fn work_takes_one_origin_and_an_optional_branch() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let parsed = |origin: &str, branch: &str| Some((origin.to_owned(), branch.to_owned()));
        assert_eq!(parse_work_args(&args(&["h/o/r"])), parsed("h/o/r", "main"));
        assert_eq!(
            parse_work_args(&args(&["--branch", "dev", "h/o/r"])),
            parsed("h/o/r", "dev")
        );
        for bad in [
            &[][..],
            &["h/o/r", "h/o/s"],
            &["h/o/r", "--branch"],
            &["h/o/r", "--origin", "x"],
        ] {
            assert_eq!(parse_work_args(&args(bad)), None, "{bad:?}");
        }
    }

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
    fn both_sessions_take_a_repository_a_branch_a_ticket_and_an_origin() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let session = |branch: Option<&str>, ticket: Option<u64>| SessionArgs {
            repo: "demo".to_owned(),
            branch: branch.map(str::to_owned),
            ticket,
            resume: None,
            origin: None,
        };
        assert_eq!(
            parse_args(&args(&["demo", "--branch", "main"])),
            Ok(session(Some("main"), None))
        );
        assert_eq!(parse_args(&args(&["demo"])), Ok(session(None, None)));
        assert_eq!(
            parse_args(&args(&["demo", "--branch", "main", "--ticket", "4"])),
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
            parse_args(&args(&["demo", "--ticket", "29", "--resume", "/s/a.jsonl"])),
            Ok(SessionArgs {
                resume: Some(PathBuf::from("/s/a.jsonl")),
                ..session(None, Some(29))
            })
        );
        assert_eq!(
            parse_args(&args(&["demo", "--resume"])),
            Err("--resume needs a session file".to_owned())
        );
        assert_eq!(
            parse_args(&args(&["demo", "--origin"])),
            Err("--origin needs an origin".to_owned())
        );
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["demo", "--branch"])).is_err());
        assert!(parse_args(&args(&["demo", "other"])).is_err());
        assert!(parse_args(&args(&["../x"])).is_err());
        assert!(parse_args(&args(&["demo", "--plan", "3"])).is_err());
        for bad in [
            &["demo", "--ticket"][..],
            &["demo", "--ticket", "0"],
            &["demo", "--ticket", "x"],
        ] {
            assert_eq!(
                parse_args(&args(bad)),
                Err("--ticket needs a ticket number".to_owned()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_master_resumes_its_own_session_and_refuses_work_of_others() {
        let f = PathBuf::from("/s/a.jsonl");
        let clean = Holds::default();
        let dirty = Holds {
            changed: 2,
            ahead: 0,
        };
        let ahead = Holds {
            changed: 0,
            ahead: 1,
        };
        let rows = [
            (clean, None, None, Start::Fresh),
            (clean, None, Some(f.clone()), Start::Resume(f.clone())),
            (dirty, Some(44), Some(f.clone()), Start::Resume(f.clone())),
            (ahead, Some(44), Some(f.clone()), Start::Resume(f.clone())),
            (dirty, Some(44), None, Start::Refuse(Refusal::SessionGone)),
            (
                dirty,
                Some(50),
                Some(f.clone()),
                Start::Refuse(Refusal::Other(50)),
            ),
            (
                dirty,
                None,
                Some(f.clone()),
                Start::Refuse(Refusal::Unrecorded),
            ),
        ];
        for (holds, recorded, earlier, expected) in rows {
            let row = format!("{holds:?} {recorded:?} {earlier:?}");
            assert_eq!(start_of(holds, recorded, 44, earlier), expected, "{row}");
        }
    }

    fn ticket(number: u64, title: &str, state: DocState, links: Vec<DocLink>) -> DocView {
        DocView {
            repo: "demo".to_owned(),
            number,
            kind: DocKind::Ticket,
            title: title.to_owned(),
            state,
            source: "user".to_owned(),
            created: 1,
            changed: 1,
            approved: None,
            released: None,
            links,
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
    fn a_ticket_brief_names_its_links_both_ways() {
        let link = |kind, number, repo: Option<&str>| DocLink {
            kind,
            number,
            repo: repo.map(str::to_owned),
        };
        let mut planned = ticket(
            3,
            "Tickets",
            DocState::Planning,
            vec![
                link(LinkKind::DependsOn, 1, None),
                link(LinkKind::Related, 9, None),
                // weida's #5 is another ticket than this repository's.
                link(LinkKind::DependsOn, 5, Some("weida")),
            ],
        );
        planned.body = "# Tickets\n\nDo it.\n\n".to_owned();
        let docs = vec![
            ticket(1, "Claims", DocState::Done, Vec::new()),
            planned.clone(),
            ticket(
                5,
                "Popout",
                DocState::Open,
                vec![link(LinkKind::DependsOn, 3, None)],
            ),
        ];
        assert_eq!(
            ticket_brief(&planned, &docs, 1),
            "# #3 Tickets\n\nDo it.\n\n## Links\n\n\
             - depends on #1 Claims (done)\n\
             - related to #9\n\
             - depends on #weida-5\n\
             - blocks #5 Popout (open)\n"
        );
        let mut bare = ticket(4, "Plan only", DocState::Planning, Vec::new());
        assert_eq!(
            ticket_brief(&bare, &[], 1),
            "# #4 Plan only\n\n(no proposal text: this ticket started as a plan)\n"
        );
        bare.comments = vec![TicketComment {
            author: "hannes@tuco-pc".to_owned(),
            at: 1,
            text: "Still needed.\nAlso on Windows.".to_owned(),
        }];
        assert!(ticket_brief(&bare, &[], 7_201).ends_with(
            "\n## Comments\n\n- hannes@tuco-pc, 2h ago:\n  Still needed.\n  Also on Windows.\n"
        ));
    }

    #[test]
    fn a_master_is_told_which_sections_were_filed_elsewhere() {
        let plain = instruction("demo", 4, "main", false, &[]);
        assert!(!plain.contains("sections"), "{plain}");
        let filed = [
            FiledTicket {
                repo: "weida".to_owned(),
                number: 7,
                title: "Keep the corpus".to_owned(),
            },
            FiledTicket {
                repo: "demo".to_owned(),
                number: 5,
                title: "Later".to_owned(),
            },
        ];
        let told = instruction("demo", 4, "main", false, &filed);
        assert!(
            told.ends_with(
                " Its sections for other tickets were filed as tickets of their own and are \
                 left out of the attached plan: #weida-7 (Keep the corpus), #5 (Later)."
            ),
            "{told}"
        );
    }
}
