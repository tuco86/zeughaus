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
//! - [`tools`]: the fourteen tools and the text they answer with;
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
use zeughaus_link::{CiReply, CiRequest, DocKind, DocState, RepoView};

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

/// `zeughaus-runner ci repo|find|doc|plan-save|planned`: one question to
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
/// - `ci plan-save [--json] <repo> <file>`: stores the plan in `file`,
///   titled by its first heading, taking up the proposals its `Proposals:`
///   line names. `--json` prints `{"repo": .., "number": ..}`.
/// - `ci planned <repo> <file>`: marks the proposals that the plan in
///   `file` names as planned, for a plan executed without being saved.
///
/// Exit 0: answered. 1: the arguments, the directory or the file were
/// wrong. 2: no CI is configured, it could not be asked, or it refused
/// (no such document, a state that cannot change). 3: `ci repo` only, no
/// repository of the CI has the checkout's origin.
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
    let client = Client::new(Link::resolve(None, state_dir));
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
        repo: String,
        file: PathBuf,
    },
    Planned {
        repo: String,
        file: PathBuf,
    },
}

/// The arguments that follow `ci`.
fn parse_ci(args: &[String]) -> Result<CiCommand, String> {
    const SUBCOMMANDS: &str = "repo, find, doc, plan-save or planned";
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(format!("ci needs a subcommand: {SUBCOMMANDS}"));
    };
    let mut json = false;
    let mut operands: Vec<&String> = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--json" if subcommand != "planned" => json = true,
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
            repo: (*repo).clone(),
            file: PathBuf::from(file),
        }),
        ("plan-save", _) => Err("ci plan-save takes a repository name and a file".to_owned()),
        ("planned", [repo, file]) => Ok(CiCommand::Planned {
            repo: (*repo).clone(),
            file: PathBuf::from(file),
        }),
        ("planned", _) => Err("ci planned takes a repository name and a file".to_owned()),
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
async fn ci_answer(client: &Client, command: CiCommand) -> Result<String, Failure> {
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
            let Some(view) = repo::ask(client, &checkout, true)
                .await
                .map_err(Failure::no_answer)?
            else {
                return Err(Failure {
                    code: EXIT_NOT_A_REPOSITORY,
                    message: format!("{} is no repository of the CI's ci.toml", checkout.origin),
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
        CiCommand::PlanSave { json, repo, file } => {
            let body = read_plan(&file)?;
            let proposals = docs::proposals_named(&body).map_err(Failure::bad_input)?;
            let request = CiRequest::DocCreate {
                repo: repo.clone(),
                kind: DocKind::Plan,
                title: docs::plan_title(&body),
                source: repo.clone(),
                body,
                proposals,
            };
            let number = match client.ci(request).await.map_err(Failure::no_answer)? {
                CiReply::DocCreated { number } => number,
                other => return Err(Failure::no_answer(unexpected(&other))),
            };
            if json {
                encoded(&SavedAnswer {
                    repo: &repo,
                    number,
                })
            } else {
                Ok(format!("saved plan #{number} of {repo}\n"))
            }
        }
        CiCommand::Planned { repo, file } => {
            let body = read_plan(&file)?;
            let numbers = docs::proposals_named(&body).map_err(Failure::bad_input)?;
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
                repo: "demo".to_owned(),
                file: PathBuf::from("/tmp/p.md")
            })
        );
        assert_eq!(
            parse_ci(&args(&["planned", "demo", "/tmp/p.md"])),
            Ok(CiCommand::Planned {
                repo: "demo".to_owned(),
                file: PathBuf::from("/tmp/p.md")
            })
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
                &["find", "zeughaus", "abc"][..],
                "is not 4 to 40 hex digits",
            ),
        ] {
            let err = parse_ci(&args(given)).expect_err(&format!("{given:?}"));
            assert!(err.contains(reason), "{given:?}: {err}");
        }
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use serde_json::{Value, json};
        use tokio::io::{AsyncReadExt, BufReader};

        /// What the agent sees of a runner that has no CI: fourteen tools, and
        /// an error with the reason from every call.
        #[tokio::test]
        async fn a_session_without_a_ci_lists_the_tools_and_explains_every_call() {
            let link = Link::resolve(None, Path::new("/nonexistent/zeughaus-state"));
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
            assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 14);
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
