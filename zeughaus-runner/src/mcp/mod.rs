//! `zeughaus-runner mcp`: the CI and manual job runs as tools of an agent.
//!
//! A Model Context Protocol server on stdio, started by the agent that uses
//! it and living as long as its session. It executes nothing and holds no
//! graph: it asks the CI, over weida, what an editor would ask it, and
//! uploads a working tree for a manual job run. It dials the CI given with
//! `--ci <url>`, else the first entry of `ci = [..]` in the state
//! directory's `zeughaus.toml`, as the principal of that directory's
//! `client.pem`: the same one an editor on this machine is.
//!
//! With no CI or no client key the server still starts and answers every
//! tool call with the reason, so a missing file is something the agent reads
//! and not a server that failed to launch.
//!
//! Nothing but protocol messages may reach stdout, so every log line goes to
//! stderr.
//!
//! `zeughaus-runner ci` puts two of these questions to the same CI for a
//! script that is no MCP client; [`cli`] answers them on stdout.
//!
//! - [`rpc`]: the JSON-RPC framing and the calls running in parallel;
//! - [`tools`]: the fifteen tools and the text they answer with;
//! - [`docs`]: plans and proposals as text, and what a plan file names;
//! - [`client`]: the CI's `/ci` and `/manual` exchanges;
//! - [`repo`]: a checkout, and the CI's view of the repository it belongs to;
//! - [`snapshot`]: a working tree and its path dependencies as git bundles.

pub(crate) mod client;
mod docs;
pub(crate) mod repo;
mod rpc;
mod snapshot;
mod tools;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::BufReader;
use zeughaus_link::ci::{plan_title, proposals_named, runs_on_named};
use zeughaus_link::{
    CiReply, CiRequest, ClaimHolder, ClaimSubject, ClaimView, DocKind, DocState, MachineView,
    RepoView, SessionView,
};

use self::client::{Client, Link};
pub(crate) use self::tools::unexpected;
use self::tools::{Toolbox, find, is_sha_prefix, one_line, pipeline_text, read_doc};

/// How long the process waits for work on the blocking pool once the session
/// is over. The pool holds the blocking read of stdin, which only ends when
/// the client closes it.
const SHUTDOWN: Duration = Duration::from_secs(2);

/// Serves one session on stdin and stdout; `args` are what follows `mcp`.
pub fn run(args: &[String], state_dir: &Path) -> ExitCode {
    let ci = match parse_args(args) {
        Ok(ci) => ci,
        Err(e) => {
            eprintln!("[mcp] {e}\nusage: zeughaus-runner [--state-dir <dir>] mcp [--ci <url>]");
            return ExitCode::FAILURE;
        }
    };
    let link = Link::resolve(ci, state_dir);
    match &link {
        Ok(link) => eprintln!("[mcp] CI {}", link.url()),
        Err(e) => eprintln!("[mcp] {e}; every tool will say so"),
    }
    // Several workers: an agent's subagents call in parallel, and a call may
    // wait minutes on the CI.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("[mcp] cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let client = Arc::new(Client::new(link));
    let toolbox = Arc::new(Toolbox::new(Arc::clone(&client)));
    let served = runtime.block_on(async {
        let served = rpc::serve(
            toolbox,
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        )
        .await;
        client.shutdown().await;
        served
    });
    runtime.shutdown_timeout(SHUTDOWN);
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[mcp] cannot read stdin: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The CI of `--ci <url>`, if given.
fn parse_args(args: &[String]) -> Result<Option<String>, String> {
    match args {
        [] => Ok(None),
        [flag, url] if flag == "--ci" => Ok(Some(url.clone())),
        [flag] if flag == "--ci" => Err("--ci needs a URL".to_owned()),
        other => Err(format!("unknown arguments {other:?}")),
    }
}

/// What `zeughaus-runner ci` exits with when it has no answer, so that a
/// script can tell the cases apart: the arguments or the directory were
/// wrong, the CI could not be asked, the CI has no such repository.
const EXIT_BAD_INPUT: u8 = 1;
const EXIT_NO_ANSWER: u8 = 2;
const EXIT_NOT_A_REPOSITORY: u8 = 3;
/// `ci wait` only: a running job reached a multiple of its expected time.
const EXIT_LATE: u8 = 4;

/// `zeughaus-runner ci <subcommand>`: one question to
/// the CI, answered on stdout, for a script that is no MCP client (an
/// agent's extension asks them at session start, after a push and when a
/// plan is saved). The CI is dialled as `mcp` does without `--ci`, and the
/// reason of every failure is one line on stderr (the first, when usage
/// follows).
///
/// - `ci repo [--json] [<path>]`: which repository of the CI the checkout
///   at `path` (default: the working directory) is, as the `ci_repo` tool
///   tells it. `--json` prints one line, `{"repo": <the CI's view>, "text":
///   <the text>}`.
/// - `ci find [--json] <repo> <sha>`: the newest pipeline of a commit, as
///   `ci_pipeline` tells it. `--json` prints the pipeline's record, or
///   `null` when there is none.
/// - `ci doc [--json] <repo> <number>`: one plan or proposal, as `doc_read`
///   tells it; `--json` prints its record with the body.
/// - `ci plan-save [--json] [--approve] <repo> <file>`: stores the plan in
///   `file`, titled by its first heading, taking up the proposals its
///   `Proposals:` line names; `--approve` approves it for the master right
///   away. `--json` prints `{"repo": .., "number": .., "approved": ..}`,
///   with `approve_failed` saying why an asked approval did not go through.
/// - `ci planned <repo> <file>`: marks the proposals that the plan in
///   `file` names as planned, for a plan executed without being saved.
/// - `ci wait <repo> <number|sha> [<job>]`: waits until the pipeline (or
///   that job of it) has settled, up to [`CLI_WAIT_LIMIT`], and prints it
///   as `ci_wait` does; an agent runs it as a background command. It ends
///   early when a running job reaches a multiple of its expected time.
/// - `ci overdue <repo> <number> <job>`: the late report of a running job
///   (how far into its expected time, its machine's other jobs, its last
///   lines), which the omp plugin's push watch sends at each multiple.
/// - `ci claim [--json] <repo> proposal|pipeline <n>`: takes the proposal
///   or pipeline for this terminal (`ZEUGHAUS_RUNNER`, `ZEUGHAUS_TERMINAL`),
///   or says who holds it. `--json` prints `{"ours": .., "claim": ..,
///   "text": ..}`.
/// - `ci renew`: renews every claim of this terminal.
/// - `ci release [<repo> proposal|pipeline <n>]`: releases one claim of
///   this terminal, or all of them.
/// - `ci responsible [--json] <repo> <n>`: who is responsible for a
///   pipeline's failure. `--json` prints `{"ours": .., "claim": ..|null,
///   "text": ..}`, `ours` when this terminal holds it.
///
/// Exit 0: answered (for `ci wait`: settled, whatever its status); an
/// `--approve` that the CI refused still exits 0 and says so, because the
/// plan is stored. 1: the arguments, the directory or the file were wrong.
/// 2: no CI is configured, it could not be asked, it refused (no such
/// document, a state that cannot change, a job that is not running or has
/// no expected time), or the wait ran out. 3: `ci repo` only, no
/// repository of the CI has the checkout's origin. 4: `ci wait` only, a
/// running job reached a multiple of its expected time; stdout says which.
pub fn cli(args: &[String], state_dir: &Path) -> ExitCode {
    let command = match parse_ci(args) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("{e}\n{}", crate::cli::USAGE);
            return ExitCode::from(EXIT_BAD_INPUT);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_NO_ANSWER);
        }
    };
    // A missing CI is the client's error on its first question, after the
    // checkout was found: a directory that is none is wrong whatever the CI
    // is doing.
    let client = Arc::new(Client::new(Link::resolve(None, state_dir)));
    let answered = runtime.block_on(async {
        let answered = ci_answer(&client, command).await;
        client.shutdown().await;
        answered
    });
    match answered {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        // The whole report, for the agent to decide on.
        Err(failure) if failure.code == EXIT_LATE => {
            print!("{}", failure.message);
            ExitCode::from(EXIT_LATE)
        }
        Err(failure) => {
            eprintln!("{}", one_line(&failure.message));
            ExitCode::from(failure.code)
        }
    }
}

/// What `zeughaus-runner ci` was asked.
#[derive(Debug, PartialEq, Eq)]
enum CiCommand {
    Repo {
        json: bool,
        path: Option<String>,
    },
    Find {
        json: bool,
        repo: String,
        sha: String,
    },
    Doc {
        json: bool,
        repo: String,
        number: u64,
    },
    PlanSave {
        json: bool,
        approve: bool,
        repo: String,
        file: PathBuf,
    },
    Planned {
        repo: String,
        file: PathBuf,
    },
    Wait {
        repo: String,
        number: Option<u64>,
        sha: Option<String>,
        job: Option<String>,
    },
    Overdue {
        repo: String,
        number: u64,
        job: String,
    },
    Session {
        json: bool,
        id: u64,
    },
    Claim {
        json: bool,
        subject: ClaimSubject,
    },
    Renew,
    Release {
        subject: Option<ClaimSubject>,
    },
    Responsible {
        json: bool,
        repo: String,
        number: u64,
    },
}

/// The holder a terminal names: a valid runner name and a terminal id, and
/// the CI session in it when there is one.
pub(crate) fn holder_of(
    runner: Option<&str>,
    terminal: Option<&str>,
    session: Option<&str>,
) -> Option<ClaimHolder> {
    let runner = runner.filter(|name| crate::names::valid_name(name))?;
    let terminal = terminal?.parse::<u64>().ok()?;
    Some(ClaimHolder {
        runner: runner.to_owned(),
        terminal,
        session: session.and_then(|id| id.parse::<u64>().ok()),
    })
}

/// The holder of the zeughaus terminal this process runs in, if it runs in
/// one.
pub(crate) fn env_holder() -> Option<ClaimHolder> {
    let var = |name: &str| std::env::var(name).ok();
    holder_of(
        var("ZEUGHAUS_RUNNER").as_deref(),
        var("ZEUGHAUS_TERMINAL").as_deref(),
        var("ZEUGHAUS_SESSION").as_deref(),
    )
}

/// What a claim is on, from `<repo> proposal|pipeline <n>`.
fn subject_of(repo: &str, kind: &str, number: &str) -> Result<ClaimSubject, String> {
    let n = match number.parse::<u64>() {
        Ok(n) if n >= 1 => n,
        _ => return Err(format!("{number:?} is no number")),
    };
    let repo = repo.to_owned();
    match kind {
        "proposal" => Ok(ClaimSubject::Proposal { repo, number: n }),
        "pipeline" => Ok(ClaimSubject::Pipeline { repo, number: n }),
        _ => Err(format!("{kind:?} is neither proposal nor pipeline")),
    }
}

/// How long `ci wait` waits: the longest a job may run by default. The
/// caller's own timeout is the hang detector.
const CLI_WAIT_LIMIT: Duration = Duration::from_secs(4 * 3600);

/// The arguments that follow `ci`.
fn parse_ci(args: &[String]) -> Result<CiCommand, String> {
    const SUBCOMMANDS: &str = "repo, find, doc, plan-save, planned, wait, overdue, session, \
                               claim, renew, release or responsible";
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(format!("ci needs a subcommand: {SUBCOMMANDS}"));
    };
    let mut json = false;
    let mut approve = false;
    let mut operands: Vec<&String> = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--json"
                if !matches!(
                    subcommand.as_str(),
                    "planned" | "wait" | "overdue" | "renew" | "release"
                ) =>
            {
                json = true
            }
            "--approve" if subcommand == "plan-save" => approve = true,
            flag if flag.len() > 1 && flag.starts_with('-') => {
                return Err(format!("unknown flag {flag:?} for ci {subcommand}"));
            }
            _ => operands.push(arg),
        }
    }
    match (subcommand.as_str(), operands.as_slice()) {
        ("repo", []) => Ok(CiCommand::Repo { json, path: None }),
        ("repo", [path]) => Ok(CiCommand::Repo {
            json,
            path: Some((*path).clone()),
        }),
        ("repo", _) => Err("ci repo takes at most one path".to_owned()),
        ("find", [repo, sha]) if is_sha_prefix(sha) => Ok(CiCommand::Find {
            json,
            repo: (*repo).clone(),
            sha: (*sha).clone(),
        }),
        ("find", [_, sha]) => Err(format!("sha {sha:?} is not 4 to 40 hex digits")),
        ("find", _) => Err("ci find takes a repository name and a sha".to_owned()),
        ("doc", [repo, number]) => match number.parse::<u64>() {
            Ok(number) if number >= 1 => Ok(CiCommand::Doc {
                json,
                repo: (*repo).clone(),
                number,
            }),
            _ => Err(format!("{number:?} is no document number")),
        },
        ("doc", _) => Err("ci doc takes a repository name and a number".to_owned()),
        ("plan-save", [repo, file]) => Ok(CiCommand::PlanSave {
            json,
            approve,
            repo: (*repo).clone(),
            file: PathBuf::from(file),
        }),
        ("plan-save", _) => Err("ci plan-save takes a repository name and a file".to_owned()),
        ("planned", [repo, file]) => Ok(CiCommand::Planned {
            repo: (*repo).clone(),
            file: PathBuf::from(file),
        }),
        ("planned", _) => Err("ci planned takes a repository name and a file".to_owned()),
        ("wait", [repo, target, rest @ ..]) if rest.len() <= 1 => {
            // Up to 7 digits is a pipeline number; more digits are a sha
            // prefix that happens to have no letter.
            let digits = target.len() <= 7 && target.bytes().all(|b| b.is_ascii_digit());
            let (number, sha) = match target.parse::<u64>() {
                Ok(number) if digits && number >= 1 => (Some(number), None),
                _ if is_sha_prefix(target) => (None, Some((*target).clone())),
                _ => {
                    return Err(format!(
                        "{target:?} is neither a pipeline number nor 4 to 40 hex digits"
                    ));
                }
            };
            let job = match rest.first() {
                Some(job) if !crate::names::valid_name(job) => {
                    return Err(format!("{job:?} is not a job name"));
                }
                job => job.map(|job| (*job).clone()),
            };
            Ok(CiCommand::Wait {
                repo: (*repo).clone(),
                number,
                sha,
                job,
            })
        }
        ("wait", _) => Err(
            "ci wait takes a repository name, a pipeline number or sha, and optionally a job"
                .to_owned(),
        ),
        ("overdue", [repo, number, job]) => {
            let number = match number.parse::<u64>() {
                Ok(number) if number >= 1 => number,
                _ => return Err(format!("{number:?} is no pipeline number")),
            };
            if !crate::names::valid_name(job) {
                return Err(format!("{job:?} is not a job name"));
            }
            Ok(CiCommand::Overdue {
                repo: (*repo).clone(),
                number,
                job: (*job).clone(),
            })
        }
        ("overdue", _) => {
            Err("ci overdue takes a repository name, a pipeline number and a job".to_owned())
        }
        ("session", [id]) => match id.parse::<u64>() {
            Ok(id) if id >= 1 => Ok(CiCommand::Session { json, id }),
            _ => Err(format!("{id:?} is no session number")),
        },
        ("session", _) => Err("ci session takes a session number".to_owned()),
        ("claim", [repo, kind, number]) => Ok(CiCommand::Claim {
            json,
            subject: subject_of(repo, kind, number)?,
        }),
        ("claim", _) => {
            Err("ci claim takes a repository name, proposal or pipeline, and a number".to_owned())
        }
        ("renew", []) => Ok(CiCommand::Renew),
        ("renew", _) => Err("ci renew takes nothing".to_owned()),
        ("release", []) => Ok(CiCommand::Release { subject: None }),
        ("release", [repo, kind, number]) => Ok(CiCommand::Release {
            subject: Some(subject_of(repo, kind, number)?),
        }),
        ("release", _) => Err(
            "ci release takes nothing, or a repository name, proposal or pipeline, and a number"
                .to_owned(),
        ),
        ("responsible", [repo, number]) => match number.parse::<u64>() {
            Ok(n) if n >= 1 => Ok(CiCommand::Responsible {
                json,
                repo: (*repo).clone(),
                number: n,
            }),
            _ => Err(format!("{number:?} is no number")),
        },
        ("responsible", _) => {
            Err("ci responsible takes a repository name and a pipeline number".to_owned())
        }
        (other, _) => Err(format!("unknown ci subcommand {other:?}: {SUBCOMMANDS}")),
    }
}

/// A `ci` command that ended without an answer: the exit code and what
/// stderr says.
#[derive(Debug, PartialEq, Eq)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn bad_input(message: String) -> Failure {
        Failure {
            code: EXIT_BAD_INPUT,
            message,
        }
    }

    fn no_answer(message: String) -> Failure {
        Failure {
            code: EXIT_NO_ANSWER,
            message,
        }
    }
}

/// The line `ci repo --json` prints.
#[derive(Serialize)]
struct RepoAnswer<'a> {
    repo: &'a RepoView,
    text: &'a str,
}

/// The line `ci plan-save --json` prints.
#[derive(Serialize)]
struct SavedAnswer<'a> {
    repo: &'a str,
    number: u64,
    approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    approve_failed: Option<String>,
}

/// The line `ci session --json` prints.
#[derive(Serialize)]
struct SessionAnswer<'a> {
    session: &'a SessionView,
    runners: &'a [MachineView],
    others: &'a [SessionView],
}

/// The line `ci claim --json` and `ci responsible --json` print.
#[derive(Serialize)]
struct ClaimAnswer<'a> {
    ours: bool,
    claim: Option<&'a ClaimView>,
    text: &'a str,
}

const NOT_IN_A_TERMINAL: &str =
    "not in a zeughaus terminal: ZEUGHAUS_RUNNER and ZEUGHAUS_TERMINAL are unset";

/// Who is responsible for pipeline `number` of `repo`, as `ci responsible`
/// says it.
fn responsible_text(repo: &str, number: u64, claim: Option<&ClaimView>) -> String {
    match claim {
        Some(claim) if claim.subject.number() == number => {
            format!("responsible for {repo} #{number}: {}", claim.holder_text())
        }
        Some(claim) => format!(
            "responsible for {repo} #{number}: {} (holds #{})",
            claim.holder_text(),
            claim.subject.number()
        ),
        None => format!("nobody holds {repo} #{number} or an earlier failure of its jobs"),
    }
}

/// What an agent session is told about itself and the runners around it.
fn session_text(session: &SessionView, runners: &[MachineView], others: &[SessionView]) -> String {
    let doc = |s: &SessionView| s.doc.map(|n| format!(" #{n}")).unwrap_or_default();
    let runner_of = |name: &str| runners.iter().find(|r| r.name == name);
    let mut text = format!(
        "session {} {} {}{} ({})\n",
        session.id,
        session.kind.name(),
        session.repo,
        doc(session),
        session.state.name()
    );
    let host = runner_of(&session.runner);
    text.push_str(&format!(
        "runs on {}: labels {}; addresses {}\n",
        session.runner,
        host.map(|r| r.labels.join(",")).unwrap_or_default(),
        host.map(|r| r.addresses.join(", ")).unwrap_or_default()
    ));
    if !session.machines.is_empty() {
        text.push_str("machines:\n");
        for machine in &session.machines {
            text.push_str(&format!(
                "  {} ssh {} labels {}\n",
                machine.name,
                machine.ssh.as_deref().unwrap_or("-"),
                machine.labels.join(",")
            ));
        }
    }
    if !others.is_empty() {
        text.push_str("other live sessions:\n");
        for other in others {
            text.push_str(&format!(
                "  #{} {} {}{} on {}\n",
                other.id,
                other.kind.name(),
                other.repo,
                doc(other),
                other.runner
            ));
        }
    }
    text.push_str("runners:\n");
    for runner in runners {
        text.push_str(&format!(
            "  {} {}{} {}\n",
            runner.name,
            runner.status,
            if runner.sessions { " sessions" } else { "" },
            runner.labels.join(",")
        ));
    }
    text
}

/// The plan in `file`; an empty one is no plan.
fn read_plan(file: &Path) -> Result<String, Failure> {
    let body = std::fs::read_to_string(file)
        .map_err(|e| Failure::bad_input(format!("cannot read {}: {e}", file.display())))?;
    if body.trim().is_empty() {
        return Err(Failure::bad_input(format!("{} is empty", file.display())));
    }
    Ok(body)
}

fn encoded<T: Serialize>(value: &T) -> Result<String, Failure> {
    serde_json::to_string(value)
        .map(|line| format!("{line}\n"))
        .map_err(|e| Failure::no_answer(format!("cannot encode the answer: {e}")))
}

/// Runs one `ci` command. `Ok` is what goes to stdout, newline included.
async fn ci_answer(client: &Arc<Client>, command: CiCommand) -> Result<String, Failure> {
    match command {
        CiCommand::Repo { json, path } => {
            let dir = match path {
                Some(path) => PathBuf::from(path),
                None => std::env::current_dir().map_err(|e| {
                    Failure::bad_input(format!("cannot read the working directory: {e}"))
                })?,
            };
            // Blocking, on the one thread this command has: nothing else
            // is waiting for it.
            let checkout = repo::checkout(&dir).map_err(Failure::bad_input)?;
            let answer = repo::ask_with(client, &checkout, true, false)
                .await
                .map_err(Failure::no_answer)?;
            let Some(view) = answer.repo else {
                return Err(Failure {
                    code: EXIT_NOT_A_REPOSITORY,
                    message: match answer.pending {
                        Some(doc) => repo::pending_text(&checkout.origin, &doc),
                        None => {
                            format!("{} is no repository of the CI's ci.toml", checkout.origin)
                        }
                    },
                });
            };
            let text = repo::describe(&view, checkout.head.as_deref());
            if json {
                let line = serde_json::to_string(&RepoAnswer {
                    repo: &view,
                    text: &text,
                })
                .map_err(|e| Failure::no_answer(format!("cannot encode the answer: {e}")))?;
                Ok(format!("{line}\n"))
            } else {
                Ok(format!("{text}\n"))
            }
        }
        CiCommand::Find { json, repo, sha } => {
            let found = find(client, &repo, &sha)
                .await
                .map_err(Failure::no_answer)?;
            if json {
                let line = serde_json::to_string(&found)
                    .map_err(|e| Failure::no_answer(format!("cannot encode the answer: {e}")))?;
                Ok(format!("{line}\n"))
            } else {
                Ok(match found {
                    Some(view) => pipeline_text(&view),
                    None => format!("no pipeline of {sha} in {repo}\n"),
                })
            }
        }
        CiCommand::Doc { json, repo, number } => {
            let doc = read_doc(client, &repo, number)
                .await
                .map_err(Failure::no_answer)?;
            if json {
                encoded(&doc)
            } else {
                Ok(format!(
                    "{}\n",
                    docs::doc_text(&doc, crate::enlist::now_secs()).trim_end()
                ))
            }
        }
        CiCommand::PlanSave {
            json,
            approve,
            repo,
            file,
        } => {
            let body = read_plan(&file)?;
            let proposals = proposals_named(&body).map_err(Failure::bad_input)?;
            let runs_on = runs_on_named(&body).map_err(Failure::bad_input)?;
            let request = CiRequest::DocCreate {
                repo: repo.clone(),
                kind: DocKind::Plan,
                title: plan_title(&body),
                source: repo.clone(),
                body,
                proposals,
                runs_on,
                onboard: None,
            };
            let number = match client.ci(request).await.map_err(Failure::no_answer)? {
                CiReply::DocCreated { number } => number,
                other => return Err(Failure::no_answer(unexpected(&other))),
            };
            // The plan is stored either way: a refused approval is said,
            // not an exit code, and the Plans tab approves it later.
            let approve_failed = if approve {
                let request = CiRequest::DocState {
                    repo: repo.clone(),
                    number,
                    state: DocState::Approved,
                };
                match client.ci(request).await {
                    Ok(CiReply::Accepted) => None,
                    Ok(other) => Some(unexpected(&other)),
                    Err(e) => Some(e),
                }
            } else {
                None
            };
            let approved = approve && approve_failed.is_none();
            if json {
                encoded(&SavedAnswer {
                    repo: &repo,
                    number,
                    approved,
                    approve_failed,
                })
            } else if approved {
                Ok(format!("saved and approved plan #{number} of {repo}\n"))
            } else if let Some(why) = approve_failed {
                Ok(format!(
                    "saved plan #{number} of {repo}; approving it failed: {why}\n"
                ))
            } else {
                Ok(format!("saved plan #{number} of {repo}\n"))
            }
        }
        CiCommand::Planned { repo, file } => {
            let body = read_plan(&file)?;
            let numbers = proposals_named(&body).map_err(Failure::bad_input)?;
            if numbers.is_empty() {
                return Ok("the plan names no proposals\n".to_owned());
            }
            for &number in &numbers {
                let request = CiRequest::DocState {
                    repo: repo.clone(),
                    number,
                    state: DocState::Planned,
                };
                match client.ci(request).await.map_err(Failure::no_answer)? {
                    CiReply::Accepted => {}
                    other => return Err(Failure::no_answer(unexpected(&other))),
                }
            }
            let marked: Vec<String> = numbers.iter().map(|n| format!("#{n}")).collect();
            Ok(format!("marked {} planned\n", marked.join(", ")))
        }
        CiCommand::Wait {
            repo,
            number,
            sha,
            job,
        } => {
            let waited = Toolbox::new(Arc::clone(client))
                .wait(Some(repo), number, sha, job, None, CLI_WAIT_LIMIT)
                .await
                .map_err(Failure::no_answer)?;
            if waited.settled {
                Ok(waited.text)
            } else if waited.late {
                Err(Failure {
                    code: EXIT_LATE,
                    message: waited.text,
                })
            } else {
                Err(Failure::no_answer(waited.text))
            }
        }
        CiCommand::Overdue { repo, number, job } => Toolbox::new(Arc::clone(client))
            .late_report(&repo, number, &job)
            .await
            .map_err(Failure::no_answer),
        CiCommand::Session { json, id } => {
            let session = match client
                .ci(CiRequest::Session { id })
                .await
                .map_err(Failure::no_answer)?
            {
                CiReply::Session { session } => session,
                other => return Err(Failure::no_answer(unexpected(&other))),
            };
            let overview = match client
                .ci(CiRequest::Overview)
                .await
                .map_err(Failure::no_answer)?
            {
                CiReply::Overview { overview } => overview,
                other => return Err(Failure::no_answer(unexpected(&other))),
            };
            let runners: Vec<MachineView> = overview
                .machines
                .into_iter()
                .filter(|m| m.kind == "runner")
                .collect();
            let others: Vec<SessionView> = overview
                .sessions
                .into_iter()
                .filter(|s| s.id != id && s.state.is_live())
                .collect();
            if json {
                encoded(&SessionAnswer {
                    session: &session,
                    runners: &runners,
                    others: &others,
                })
            } else {
                Ok(session_text(&session, &runners, &others))
            }
        }
        CiCommand::Claim { json, subject } => {
            let holder =
                env_holder().ok_or_else(|| Failure::bad_input(NOT_IN_A_TERMINAL.to_owned()))?;
            let request = CiRequest::Claim {
                subject: subject.clone(),
                holder,
            };
            let (claim, ours) = match client.ci(request).await.map_err(Failure::no_answer)? {
                CiReply::Claim { claim, ours } => (claim, ours),
                other => return Err(Failure::no_answer(unexpected(&other))),
            };
            let text = if ours {
                format!("claimed {}", subject.text())
            } else {
                format!("{} is held by {}", subject.text(), claim.holder_text())
            };
            if json {
                encoded(&ClaimAnswer {
                    ours,
                    claim: Some(&claim),
                    text: &text,
                })
            } else {
                Ok(format!("{text}\n"))
            }
        }
        CiCommand::Renew => {
            let holder =
                env_holder().ok_or_else(|| Failure::bad_input(NOT_IN_A_TERMINAL.to_owned()))?;
            match client
                .ci(CiRequest::ClaimRenew { holder })
                .await
                .map_err(Failure::no_answer)?
            {
                CiReply::Accepted => Ok("renewed\n".to_owned()),
                other => Err(Failure::no_answer(unexpected(&other))),
            }
        }
        CiCommand::Release { subject } => {
            let holder =
                env_holder().ok_or_else(|| Failure::bad_input(NOT_IN_A_TERMINAL.to_owned()))?;
            match client
                .ci(CiRequest::ClaimRelease { holder, subject })
                .await
                .map_err(Failure::no_answer)?
            {
                CiReply::Accepted => Ok("released\n".to_owned()),
                other => Err(Failure::no_answer(unexpected(&other))),
            }
        }
        CiCommand::Responsible { json, repo, number } => {
            let request = CiRequest::Responsible {
                repo: repo.clone(),
                number,
            };
            let claim = match client.ci(request).await.map_err(Failure::no_answer)? {
                CiReply::Responsible { claim } => claim,
                other => return Err(Failure::no_answer(unexpected(&other))),
            };
            let text = responsible_text(&repo, number, claim.as_ref());
            if json {
                let ours = env_holder()
                    .zip(claim.as_ref())
                    .is_some_and(|(holder, claim)| claim.holder.same(&holder));
                encoded(&ClaimAnswer {
                    ours,
                    claim: claim.as_ref(),
                    text: &text,
                })
            } else {
                Ok(format!("{text}\n"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mcp_takes_an_optional_ci() {
        assert_eq!(parse_args(&args(&[])), Ok(None));
        assert_eq!(
            parse_args(&args(&["--ci", "weida://x/"])),
            Ok(Some("weida://x/".to_owned()))
        );
        assert!(parse_args(&args(&["--ci"])).is_err());
        assert!(parse_args(&args(&["--ci", "a", "b"])).is_err());
        assert!(parse_args(&args(&["--feed-addr", "x"])).is_err());
    }

    #[test]
    fn ci_takes_repo_or_find_and_a_json_flag_wherever_it_stands() {
        assert_eq!(
            parse_ci(&args(&["repo"])),
            Ok(CiCommand::Repo {
                json: false,
                path: None
            })
        );
        for given in [
            &["repo", "--json", "/work/x"][..],
            &["repo", "/work/x", "--json"][..],
        ] {
            assert_eq!(
                parse_ci(&args(given)),
                Ok(CiCommand::Repo {
                    json: true,
                    path: Some("/work/x".to_owned())
                })
            );
        }
        assert_eq!(
            parse_ci(&args(&["find", "zeughaus", "3635DAD"])),
            Ok(CiCommand::Find {
                json: false,
                repo: "zeughaus".to_owned(),
                sha: "3635DAD".to_owned()
            })
        );
        assert_eq!(
            parse_ci(&args(&["find", "--json", "zeughaus", "3635dad"])),
            Ok(CiCommand::Find {
                json: true,
                repo: "zeughaus".to_owned(),
                sha: "3635dad".to_owned()
            })
        );
        assert_eq!(
            parse_ci(&args(&["doc", "--json", "demo", "3"])),
            Ok(CiCommand::Doc {
                json: true,
                repo: "demo".to_owned(),
                number: 3
            })
        );
        assert_eq!(
            parse_ci(&args(&["plan-save", "demo", "/tmp/p.md", "--json"])),
            Ok(CiCommand::PlanSave {
                json: true,
                approve: false,
                repo: "demo".to_owned(),
                file: PathBuf::from("/tmp/p.md")
            })
        );
        assert_eq!(
            parse_ci(&args(&["plan-save", "--approve", "demo", "f"])),
            Ok(CiCommand::PlanSave {
                json: false,
                approve: true,
                repo: "demo".to_owned(),
                file: PathBuf::from("f")
            })
        );
        assert_eq!(
            parse_ci(&args(&["planned", "demo", "/tmp/p.md"])),
            Ok(CiCommand::Planned {
                repo: "demo".to_owned(),
                file: PathBuf::from("/tmp/p.md")
            })
        );
        let wait = |number: Option<u64>, sha: Option<&str>, job: Option<&str>| CiCommand::Wait {
            repo: "demo".to_owned(),
            number,
            sha: sha.map(str::to_owned),
            job: job.map(str::to_owned),
        };
        assert_eq!(
            parse_ci(&args(&["wait", "demo", "12"])),
            Ok(wait(Some(12), None, None))
        );
        assert_eq!(
            parse_ci(&args(&["wait", "demo", "deadbeef", "build"])),
            Ok(wait(None, Some("deadbeef"), Some("build")))
        );
        // Eight digits are more than a pipeline number has: a sha prefix.
        assert_eq!(
            parse_ci(&args(&["wait", "demo", "12345678"])),
            Ok(wait(None, Some("12345678"), None))
        );
    }

    #[test]
    fn ci_refuses_what_it_does_not_know_with_the_reason_first() {
        for (given, reason) in [
            (&[][..], "ci needs a subcommand"),
            (&["status"][..], "unknown ci subcommand \"status\""),
            (&["repo", "a", "b"][..], "ci repo takes at most one path"),
            (
                &["repo", "--jsno"][..],
                "unknown flag \"--jsno\" for ci repo",
            ),
            (&["find"][..], "ci find takes a repository name and a sha"),
            (&["find", "zeughaus"][..], "ci find takes"),
            (&["find", "a", "b", "c"][..], "ci find takes"),
            (
                &["find", "zeughaus", "xyz1"][..],
                "is not 4 to 40 hex digits",
            ),
            (&["doc", "demo", "0"][..], "\"0\" is no document number"),
            (&["doc", "demo"][..], "ci doc takes"),
            (&["plan-save", "demo"][..], "ci plan-save takes"),
            (
                &["planned", "--json", "demo", "f"][..],
                "unknown flag \"--json\" for ci planned",
            ),
            (
                &["doc", "--approve", "demo", "1"][..],
                "unknown flag \"--approve\" for ci doc",
            ),
            (
                &["find", "zeughaus", "abc"][..],
                "is not 4 to 40 hex digits",
            ),
            (
                &["wait", "demo", "xyz"][..],
                "\"xyz\" is neither a pipeline number nor 4 to 40 hex digits",
            ),
            (&["wait", "demo", "12", "a", "b"][..], "ci wait takes"),
            (&["wait", "demo"][..], "ci wait takes"),
            (
                &["wait", "demo", "12", "Bad"][..],
                "\"Bad\" is not a job name",
            ),
            (
                &["wait", "--json", "demo", "12"][..],
                "unknown flag \"--json\" for ci wait",
            ),
            (
                &["claim", "demo", "plan", "3"][..],
                "\"plan\" is neither proposal nor pipeline",
            ),
            (
                &["claim", "demo", "pipeline", "0"][..],
                "\"0\" is no number",
            ),
            (&["claim", "demo", "3"][..], "ci claim takes"),
            (&["renew", "x"][..], "ci renew takes nothing"),
            (
                &["renew", "--json"][..],
                "unknown flag \"--json\" for ci renew",
            ),
            (
                &["release", "--json"][..],
                "unknown flag \"--json\" for ci release",
            ),
            (&["release", "demo"][..], "ci release takes"),
            (&["responsible", "demo", "x"][..], "\"x\" is no number"),
            (&["responsible", "demo"][..], "ci responsible takes"),
        ] {
            let err = parse_ci(&args(given)).expect_err(&format!("{given:?}"));
            assert!(err.contains(reason), "{given:?}: {err}");
        }
    }

    #[test]
    fn ci_claims_a_proposal_or_pipeline_and_asks_who_is_responsible() {
        assert_eq!(
            parse_ci(&args(&["claim", "--json", "demo", "proposal", "3"])),
            Ok(CiCommand::Claim {
                json: true,
                subject: ClaimSubject::Proposal {
                    repo: "demo".to_owned(),
                    number: 3
                }
            })
        );
        assert_eq!(parse_ci(&args(&["renew"])), Ok(CiCommand::Renew));
        assert_eq!(
            parse_ci(&args(&["release"])),
            Ok(CiCommand::Release { subject: None })
        );
        assert_eq!(
            parse_ci(&args(&["release", "demo", "pipeline", "12"])),
            Ok(CiCommand::Release {
                subject: Some(ClaimSubject::Pipeline {
                    repo: "demo".to_owned(),
                    number: 12
                })
            })
        );
        assert_eq!(
            parse_ci(&args(&["responsible", "demo", "12", "--json"])),
            Ok(CiCommand::Responsible {
                json: true,
                repo: "demo".to_owned(),
                number: 12
            })
        );
    }

    #[test]
    fn a_holder_needs_a_valid_runner_and_a_numeric_terminal() {
        assert_eq!(
            holder_of(Some("atik"), Some("7"), Some("38")),
            Some(ClaimHolder {
                runner: "atik".to_owned(),
                terminal: 7,
                session: Some(38)
            })
        );
        assert_eq!(
            holder_of(Some("atik"), Some("7"), None).map(|h| h.session),
            Some(None)
        );
        assert_eq!(holder_of(Some("atik"), None, None), None);
        assert_eq!(holder_of(Some("atik"), Some("seven"), None), None);
        assert_eq!(holder_of(Some("Atik PC"), Some("7"), None), None);
        assert_eq!(holder_of(None, Some("7"), None), None);
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use serde_json::{Value, json};
        use tokio::io::{AsyncReadExt, BufReader};

        /// What the agent sees of a runner that has no CI: fifteen tools, and
        /// an error with the reason from every call.
        #[tokio::test]
        async fn a_session_without_a_ci_lists_the_tools_and_explains_every_call() {
            let link = Link::resolve_given(None, Path::new("/nonexistent/zeughaus-state"));
            let client = Arc::new(Client::new(link));
            let toolbox = Arc::new(Toolbox::new(client));
            let script = concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ci_status","arguments":{}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nothing"}}"#,
                "\n",
            );
            let (mut read, write) = tokio::io::duplex(1 << 20);
            rpc::serve(toolbox, BufReader::new(script.as_bytes()), write)
                .await
                .expect("served");
            let mut written = String::new();
            read.read_to_string(&mut written).await.expect("answers");
            let answers: Vec<Value> = written
                .lines()
                .map(|line| serde_json::from_str(line).expect("JSON"))
                .collect();
            let by_id = |id: u64| {
                answers
                    .iter()
                    .find(|answer| answer["id"] == json!(id))
                    .unwrap_or_else(|| panic!("no answer for {id}: {written}"))
            };

            assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "zeughaus");
            assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 19);
            let status = &by_id(3)["result"];
            assert_eq!(status["isError"], true);
            let text = status["content"][0]["text"].as_str().unwrap();
            assert!(
                text.starts_with("no CI configured: add ci = [..] to /nonexistent/zeughaus-state"),
                "{text}"
            );
            assert_eq!(by_id(4)["result"]["isError"], true);
            assert_eq!(answers.len(), 4, "the notification is not answered");
        }
    }
}
