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
//! - [`tools`]: the ten tools and the text they answer with;
//! - [`client`]: the CI's `/ci` and `/manual` exchanges;
//! - [`repo`]: a checkout, and the CI's view of the repository it belongs to;
//! - [`snapshot`]: a working tree and its path dependencies as git bundles.

mod client;
mod repo;
mod rpc;
mod snapshot;
mod tools;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::BufReader;
use zeughaus_link::RepoView;

use self::client::{Client, Link};
use self::tools::{Toolbox, find, is_sha_prefix, one_line, pipeline_text};

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

/// `zeughaus-runner ci repo|find`: one question to the CI, answered on
/// stdout, for a script that is no MCP client (an agent's extension asks
/// them at session start and after a push). The CI is dialled as `mcp` does
/// without `--ci`, and the reason of every failure is one line on stderr
/// (the first, when usage follows).
///
/// - `ci repo [--json] [<path>]`: which repository of the CI the checkout
///   at `path` (default: the working directory) is, as the `ci_repo` tool
///   tells it. `--json` prints one line, `{"repo": <the CI's view>, "text":
///   <the text>}`.
/// - `ci find [--json] <repo> <sha>`: the newest pipeline of a commit, as
///   `ci_pipeline` tells it. `--json` prints the pipeline's record, or
///   `null` when there is none.
///
/// Exit 0: answered. 1: the arguments or the directory were wrong. 2: no CI
/// is configured, or it could not be asked. 3: `ci repo` only, no repository
/// of the CI has the checkout's origin.
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
}

/// The arguments that follow `ci`.
fn parse_ci(args: &[String]) -> Result<CiCommand, String> {
    let Some((subcommand, rest)) = args.split_first() else {
        return Err("ci needs a subcommand: repo or find".to_owned());
    };
    let mut json = false;
    let mut operands: Vec<&String> = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--json" => json = true,
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
        (other, _) => Err(format!("unknown ci subcommand {other:?}: repo or find")),
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
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, BufReader};

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
            (
                &["find", "zeughaus", "abc"][..],
                "is not 4 to 40 hex digits",
            ),
        ] {
            let err = parse_ci(&args(given)).expect_err(&format!("{given:?}"));
            assert!(err.contains(reason), "{given:?}: {err}");
        }
    }

    /// What the agent sees of a runner that has no CI: ten tools, and an
    /// error with the reason from every call.
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
        assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 10);
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
