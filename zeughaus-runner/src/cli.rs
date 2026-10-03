//! The runner binary as a client of a running runner: `trigger` and `hold`.
//!
//! Both talk to the endpoint a runner printed at start (`[runner] weida
//! endpoint weida://sha256:<fp>@host:port/`) with the client identity of this
//! machine's state directory, the same one an editor presents. No store is
//! involved: a script, a cron entry or a webhook relay names the runner it
//! means, and the fingerprint in the URL is what makes that safe.
//!
//! ```text
//! zeughaus-runner trigger <endpoint> <node-id> [payload]
//! zeughaus-runner hold <endpoint> on|off
//! ```

use std::path::Path;
use std::process::ExitCode;

use weida::{ClientTls, EndpointAddr, Runtime, RuntimeConfig, TransferMeta, Trust};
use zeughaus_link::{
    HOLD_PATH, HoldReply, HoldRequest, MAX_HOLD_BYTES, TRIGGERS_PATH, TriggerRequest, credentials,
};

/// What the binary accepts. A runner serves on whatever state directory it
/// is given, so an argument it does not know must never fall through to
/// serving: a second runner on the same state directory takes the first
/// one's terminals.
const USAGE: &str = "\
usage: zeughaus-runner [--state-dir <dir>] [--feed-addr <host:port>] [--keep-runs <n>] [join <host[:port]/database>]
       zeughaus-runner trigger <endpoint> <node-id> [payload]
       zeughaus-runner hold <endpoint> on|off
       zeughaus-runner ci check|plan|run|status|forge-check|hook ...";

/// Runs a subcommand if the first argument names one. `None` means the
/// arguments are the runner's own and the process should serve.
///
/// `--state-dir <path>` is the one flag both modes share; it is already
/// resolved into `state_dir` and is removed here so it can stand anywhere.
pub fn run(args: &[String], state_dir: &Path) -> Option<ExitCode> {
    let mut positional: Vec<String> = Vec::with_capacity(args.len());
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--state-dir" {
            rest.next();
        } else {
            positional.push(arg.clone());
        }
    }
    let command = match positional.first().map(String::as_str) {
        // CI subcommands work on the state directory, not on a running
        // runner's endpoint, and report their own outcome.
        Some("ci") => return Some(crate::ci::cli::run(&positional[1..], state_dir)),
        Some("trigger") => parse_trigger(&positional[1..]),
        Some("hold") => parse_hold(&positional[1..]),
        _ => {
            return match check_serve_args(&positional) {
                Ok(ServeArgs::Serve) => None,
                Ok(ServeArgs::Help) => {
                    println!("{USAGE}");
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("[runner] {e}\n{USAGE}");
                    Some(ExitCode::FAILURE)
                }
            };
        }
    };
    let command = match command {
        Ok(command) => command,
        Err(e) => {
            eprintln!("[runner] {e}");
            return Some(ExitCode::FAILURE);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[runner] cannot start the async runtime: {e}");
            return Some(ExitCode::FAILURE);
        }
    };
    let outcome = rt.block_on(command.send(state_dir));
    Some(match outcome {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[runner] {e}");
            ExitCode::FAILURE
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum ServeArgs {
    Serve,
    Help,
}

/// Whether `args` (without `--state-dir`) are the serving mode's own. The
/// values are parsed where they are used; this only refuses what no mode
/// reads.
fn check_serve_args(args: &[String]) -> Result<ServeArgs, String> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => return Ok(ServeArgs::Help),
            "--feed-addr" | "--keep-runs" | "join" => {
                if rest.next().is_none() {
                    return Err(format!("{arg} needs a value"));
                }
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(ServeArgs::Serve)
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Trigger {
        endpoint: String,
        request: TriggerRequest,
    },
    Hold {
        endpoint: String,
        held: bool,
    },
}

fn parse_trigger(args: &[String]) -> Result<Command, String> {
    let [endpoint, node_id, rest @ ..] = args else {
        return Err("usage: zeughaus-runner trigger <endpoint> <node-id> [payload]".to_owned());
    };
    let node_id = node_id
        .parse::<u64>()
        .map_err(|_| format!("node id {node_id:?} is not a number"))?;
    let payload = match rest {
        [] => None,
        [payload] => Some(payload.clone()),
        _ => return Err("trigger takes at most one payload argument".to_owned()),
    };
    // A press from a script or a hook: nobody in an editor asked for the
    // run, so it is shown where every editor sees it.
    Ok(Command::Trigger {
        endpoint: endpoint.clone(),
        request: TriggerRequest {
            node_id,
            payload,
            external: true,
        },
    })
}

fn parse_hold(args: &[String]) -> Result<Command, String> {
    let [endpoint, state] = args else {
        return Err("usage: zeughaus-runner hold <endpoint> on|off".to_owned());
    };
    let held = match state.as_str() {
        "on" => true,
        "off" => false,
        other => return Err(format!("hold takes on or off, not {other:?}")),
    };
    Ok(Command::Hold {
        endpoint: endpoint.clone(),
        held,
    })
}

/// The URL of `path` on the announced root endpoint.
fn url_of(endpoint: &str, path: &str) -> Result<String, String> {
    let mut addr =
        EndpointAddr::parse(endpoint).map_err(|e| format!("endpoint {endpoint}: {e}"))?;
    addr.path = path.to_owned();
    Ok(addr.to_string())
}

impl Command {
    /// What is printed on success.
    async fn send(self, state_dir: &Path) -> Result<String, String> {
        let identity = credentials::load_client_identity(state_dir).ok_or_else(|| {
            format!(
                "no client identity in {}: a runner that requires a client refuses \
                 this dial (start a runner under this state directory once, or copy \
                 its client.pem here)",
                state_dir.display()
            )
        })?;
        let tls = ClientTls::new(Trust::by_address()).with_identity(identity);
        let runtime = Runtime::new(RuntimeConfig::default()).map_err(|e| format!("weida: {e}"))?;
        let result = match self {
            Command::Trigger { endpoint, request } => {
                let url = url_of(&endpoint, TRIGGERS_PATH)?;
                let pusher = runtime.pusher(tls);
                pusher
                    .connect(&url)
                    .await
                    .map_err(|e| format!("connect {url}: {e}"))?;
                let node_id = request.node_id;
                // The receipt matters here, unlike for an editor's press:
                // this process exits right after, and a push whose bytes are
                // still in flight would be discarded with the connection.
                let mut transfer = pusher
                    .open(TransferMeta::default())
                    .await
                    .map_err(|e| format!("trigger {node_id}: {e}"))?;
                transfer
                    .write_all(&request.encode())
                    .await
                    .map_err(|e| format!("trigger {node_id}: {e}"))?;
                transfer
                    .finish()
                    .map_err(|e| format!("trigger {node_id}: {e}"))?
                    .delivered()
                    .await
                    .map_err(|e| format!("trigger {node_id}: not delivered: {e}"))?;
                Ok(format!("triggered node {node_id}"))
            }
            Command::Hold { endpoint, held } => {
                let url = url_of(&endpoint, HOLD_PATH)?;
                let requester = runtime.requester(tls);
                requester
                    .connect(&url)
                    .await
                    .map_err(|e| format!("connect {url}: {e}"))?;
                let reply = requester
                    .request(&HoldRequest { held }.encode())
                    .await
                    .map_err(|e| format!("hold: {e}"))?;
                let encoded = reply
                    .collect(MAX_HOLD_BYTES)
                    .await
                    .map_err(|e| format!("hold: {e}"))?;
                let reply = HoldReply::decode(&encoded).ok_or("hold: malformed reply")?;
                Ok(format!(
                    "runner {}; {} run(s) live",
                    if reply.held { "held" } else { "released" },
                    reply.live_runs
                ))
            }
        };
        // A push is acknowledged only by the connection closing cleanly; a
        // shutdown that is skipped would leave the trigger in a socket the
        // process exits out from under.
        runtime.shutdown().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The push must be in the runner's hands when `send` returns: the
    /// process exits right after, and the first version of this command lost
    /// every trigger by closing the connection with the bytes still in
    /// flight. Real loopback QUIC with the runner's own listener.
    #[tokio::test]
    async fn a_trigger_is_delivered_before_send_returns() {
        let state = std::env::temp_dir().join(format!("zeughaus-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let transport = crate::transport::Transport::start(
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            &state,
        )
        .await
        .expect("listener");
        let puller = transport
            .listener()
            .puller(TRIGGERS_PATH)
            .expect("trigger endpoint");
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tokio::spawn(crate::transport::accept_triggers(puller, tx));

        let command = parse_trigger(&[
            transport.url().to_string(),
            "42".to_string(),
            "payload".to_string(),
        ])
        .expect("parsed");
        let line = command.send(&state).await.expect("sent");
        assert_eq!(line, "triggered node 42");

        // Already queued, or the send returned before delivery.
        let received =
            tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(5)))
                .await
                .expect("join")
                .expect("the press reached the runner");
        assert_eq!(
            received,
            TriggerRequest {
                node_id: 42,
                payload: Some("payload".to_string()),
                external: true,
            }
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn trigger_takes_an_optional_payload() {
        let bare = parse_trigger(&args(&["weida://x/", "7"])).unwrap();
        assert_eq!(
            bare,
            Command::Trigger {
                endpoint: "weida://x/".into(),
                request: TriggerRequest {
                    node_id: 7,
                    payload: None,
                    external: true,
                }
            }
        );
        let with = parse_trigger(&args(&["weida://x/", "7", "{\"ref\":\"main\"}"])).unwrap();
        assert_eq!(
            with,
            Command::Trigger {
                endpoint: "weida://x/".into(),
                request: TriggerRequest {
                    node_id: 7,
                    payload: Some("{\"ref\":\"main\"}".into()),
                    external: true,
                }
            }
        );
        assert!(parse_trigger(&args(&["weida://x/", "seven"])).is_err());
        assert!(parse_trigger(&args(&["weida://x/"])).is_err());
        assert!(parse_trigger(&args(&["weida://x/", "7", "a", "b"])).is_err());
    }

    #[test]
    fn hold_is_on_or_off() {
        assert_eq!(
            parse_hold(&args(&["weida://x/", "on"])).unwrap(),
            Command::Hold {
                endpoint: "weida://x/".into(),
                held: true
            }
        );
        assert_eq!(
            parse_hold(&args(&["weida://x/", "off"])).unwrap(),
            Command::Hold {
                endpoint: "weida://x/".into(),
                held: false
            }
        );
        assert!(parse_hold(&args(&["weida://x/", "maybe"])).is_err());
        assert!(parse_hold(&args(&["weida://x/"])).is_err());
    }

    #[test]
    fn only_the_serving_flags_serve() {
        // The shapes the installed units start the runner with.
        for serve in [
            &[][..],
            &["--feed-addr", "10.8.0.10:7443"][..],
            &["--keep-runs", "20", "join", "127.0.0.1:3000/zeughaus"][..],
        ] {
            assert_eq!(check_serve_args(&args(serve)), Ok(ServeArgs::Serve));
        }
        assert_eq!(check_serve_args(&args(&["--help"])), Ok(ServeArgs::Help));
        assert_eq!(
            check_serve_args(&args(&["join", "x", "-h"])),
            Ok(ServeArgs::Help)
        );
        assert!(check_serve_args(&args(&["--version"])).is_err());
        assert!(check_serve_args(&args(&["status"])).is_err());
        assert!(check_serve_args(&args(&["join"])).is_err());
    }
}
