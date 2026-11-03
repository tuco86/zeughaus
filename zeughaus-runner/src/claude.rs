//! `zeughaus-runner claude login|status`: the CI's Claude sign-in, from the
//! machine the browser runs on.
//!
//! `login` asks the CI to start a sign-in (`CiRequest::ClaudeStart`: the CI
//! keeps state and PKCE verifier), opens the browser at the claude.ai
//! address it answers, takes the redirect on `localhost:54545`, the only
//! one Claude Code's OAuth client accepts, and hands code and state to the
//! CI (`CiRequest::ClaudeFinish`), which exchanges them and from then on
//! renews the token and hands it out through OpenBao. It dials the CI the
//! way an editor of this state directory does, with its `client.pem`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use zeughaus_link::{CiReply, CiRequest, query_param};

use crate::mcp::client::{Client, Link};

/// The callback's port: `zeughaus_link::CLAUDE_CALLBACK`.
const PORT: u16 = 54545;
/// The callback's path.
const PATH: &str = "/callback";
/// How long the browser has to come back.
const WAIT: Duration = Duration::from_secs(600);
/// The most of a request line and headers read.
const MAX_HEAD: usize = 16 << 10;

const USAGE: &str =
    "usage: zeughaus-runner claude login [--ci <url>] [--no-browser] | status [--ci <url>]";

pub fn run(args: &[String], state_dir: &Path) -> ExitCode {
    let mut ci = None;
    let mut browser = true;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--ci" => match rest.next() {
                Some(url) => ci = Some(url.clone()),
                None => return fail("--ci needs the CI's URL".to_owned()),
            },
            "--no-browser" => browser = false,
            _ => return fail(USAGE.to_owned()),
        }
    }
    let result = match args.first().map(String::as_str) {
        Some("login") => login(ci, state_dir, browser),
        Some("status") => status(ci, state_dir),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn fail(message: String) -> ExitCode {
    eprintln!("[claude] {message}");
    ExitCode::FAILURE
}

/// One question to the CI.
fn ask(ci: Option<String>, state_dir: &Path, request: CiRequest) -> Result<CiReply, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let client = Client::new(Link::resolve(ci, state_dir));
    runtime.block_on(async {
        let asked = client.ci(request).await;
        client.shutdown().await;
        asked
    })
}

fn login(ci: Option<String>, state_dir: &Path, browser: bool) -> Result<String, String> {
    // Bound before the CI is asked: a port that is taken fails at once.
    let listeners = bind()?;
    let url = match ask(ci.clone(), state_dir, CiRequest::ClaudeStart)? {
        CiReply::ClaudeStart { url } => url,
        other => return Err(crate::mcp::unexpected(&other)),
    };
    eprintln!("[claude] sign in at {url}");
    if browser && let Err(e) = open(&url) {
        eprintln!("[claude] no browser opened ({e}); open the address above");
    }
    let (sender, received) = mpsc::channel();
    for listener in listeners {
        let sender = sender.clone();
        std::thread::spawn(move || accept(&listener, &sender));
    }
    let (query, mut stream) = received.recv_timeout(WAIT).map_err(|_| {
        format!(
            "the browser did not come back within {} minutes",
            WAIT.as_secs() / 60
        )
    })?;
    let finished = match (query_param(&query, "code"), query_param(&query, "state")) {
        _ if query_param(&query, "error").is_some() => Err(format!(
            "claude.ai refused: {}",
            query_param(&query, "error_description")
                .or_else(|| query_param(&query, "error"))
                .unwrap_or_default()
        )),
        (Some(code), Some(state)) => {
            match ask(ci, state_dir, CiRequest::ClaudeFinish { code, state }) {
                Ok(CiReply::ClaudeSignedIn { account }) => Ok(account),
                Ok(other) => Err(crate::mcp::unexpected(&other)),
                Err(e) => Err(e),
            }
        }
        _ => Err("the redirect carries no code and state".to_owned()),
    };
    let page = match &finished {
        Ok(account) => {
            format!("The CI is signed in to Claude as {account}. This tab can be closed.")
        }
        Err(e) => format!("The sign-in failed: {e}"),
    };
    respond(&mut stream, 200, &page);
    finished.map(|account| format!("the CI is signed in to Claude as {account}"))
}

fn status(ci: Option<String>, state_dir: &Path) -> Result<String, String> {
    let overview = match ask(ci, state_dir, CiRequest::Overview)? {
        CiReply::Overview { overview } => overview,
        other => return Err(crate::mcp::unexpected(&other)),
    };
    let Some(claude) = overview.claude else {
        return Ok("the CI has no [claude]".to_owned());
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut line = match &claude.account {
        Some(account) => format!(
            "signed in as {account}; the sign-in ends in {} days, the token in {} minutes",
            claude.ends.saturating_sub(now) / 86_400,
            claude.expires.saturating_sub(now) / 60
        ),
        None => "signed out; `zeughaus-runner claude login` signs in".to_owned(),
    };
    if let Some(error) = claude.error {
        line.push_str(&format!("\nlast error: {error}"));
    }
    Ok(line)
}

/// The callback's listeners: IPv4, and IPv6 where there is one, since a
/// browser may resolve `localhost` to either.
fn bind() -> Result<Vec<TcpListener>, String> {
    let v4 = TcpListener::bind(("127.0.0.1", PORT)).map_err(|e| {
        format!("cannot listen on localhost:{PORT} ({e}); is another sign-in running?")
    })?;
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", PORT)) {
        listeners.push(v6);
    }
    Ok(listeners)
}

/// Hands the query of the first `GET /callback` to `sender`, with the
/// connection to answer; anything else gets a 404.
fn accept(listener: &TcpListener, sender: &mpsc::Sender<(String, TcpStream)>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        match callback_query(&read_head(&mut stream)) {
            Some(query) => {
                let _ = sender.send((query, stream));
                return;
            }
            None => respond(&mut stream, 404, "Not the sign-in's callback."),
        }
    }
}

/// The request line and headers, as far as they come.
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut buffer = [0u8; 2048];
    while head.len() < MAX_HEAD && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// The query of a `GET /callback?...` request head.
fn callback_query(head: &str) -> Option<String> {
    let line = head.lines().next()?;
    let mut parts = line.split(' ');
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    (path == PATH).then(|| query.to_owned())
}

fn respond(stream: &mut TcpStream, code: u16, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Claude sign-in</title>\
         <p style=\"font:15px sans-serif;margin:3em\">{}</p>",
        text.replace('&', "&amp;").replace('<', "&lt;")
    );
    let reason = if code == 200 { "OK" } else { "Not Found" };
    let _ = write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Opens `url` in the desktop's browser.
fn open(url: &str) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_get_of_the_callback_carries_the_query() {
        assert_eq!(
            callback_query("GET /callback?code=c&state=s HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Some("code=c&state=s".to_owned())
        );
        assert_eq!(callback_query("GET /favicon.ico HTTP/1.1\r\n\r\n"), None);
        assert_eq!(
            callback_query("POST /callback?code=c HTTP/1.1\r\n\r\n"),
            None
        );
    }
}
