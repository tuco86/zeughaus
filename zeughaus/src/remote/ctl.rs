//! `zeughaus ctl <socket> <words...>`: one command line to a headless editor,
//! its reply on stdout.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Sends the words joined by spaces and prints the reply. Exits 0 on `ok`,
/// 1 on `err` or when the host cannot be reached, 2 on a bad command line.
/// A relative `screenshot` or `record` path is resolved against this
/// process's working directory, not the host's.
pub fn run(args: &[String]) -> i32 {
    let [socket, words @ ..] = args else {
        eprintln!("usage: zeughaus ctl <socket> <command> [args...]");
        return 2;
    };
    if words.is_empty() {
        eprintln!("usage: zeughaus ctl <socket> <command> [args...]");
        return 2;
    }
    let line = match words {
        [name, rest @ ..] if (name == "screenshot" || name == "record") && !rest.is_empty() => {
            let path = PathBuf::from(rest.join(" "));
            let path = if path.is_relative() {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(path),
                    Err(e) => {
                        println!("err cwd: {e}");
                        return 1;
                    }
                }
            } else {
                path
            };
            format!("{name} {}", path.display())
        }
        _ => words.join(" "),
    };
    match exchange(socket, &line) {
        Ok(reply) => {
            println!("{reply}");
            if reply == "ok" || reply.starts_with("ok ") {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("err {socket}: {e}");
            1
        }
    }
}

fn exchange(socket: &str, line: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "host closed the connection without a reply",
        ));
    }
    Ok(reply.trim_end().to_owned())
}
