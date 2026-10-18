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
//! - [`rpc`]: the JSON-RPC framing and the calls running in parallel;
//! - [`tools`]: the nine tools and the text they answer with;
//! - [`client`]: the CI's `/ci` and `/manual` exchanges;
//! - [`snapshot`]: a working tree and its path dependencies as git bundles.

mod client;
mod rpc;
mod snapshot;
mod tools;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;

use self::client::{Client, Link};
use self::tools::Toolbox;

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

    /// What the agent sees of a runner that has no CI: nine tools, and an
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
        assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 9);
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
