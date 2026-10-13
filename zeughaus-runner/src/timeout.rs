//! `zeughaus-runner timeout <seconds> -- <program> [args..]`: a program with a
//! deadline.
//!
//! A host CI job runs under this because macOS has no coreutils `timeout`. It
//! follows the coreutils contract the container jobs get: the child runs in a
//! process group of its own, the group gets `SIGTERM` at the deadline and
//! `SIGKILL` a minute later, and the exit status is 124 when the deadline
//! ended the run, otherwise the child's code (`128 + signal` for a signal).
//! A `SIGHUP`, `SIGINT` or `SIGTERM` sent to this process goes to the child's
//! group too: the group is not the terminal's foreground group, so nothing
//! else would deliver it.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// How long a group has to end after `SIGTERM` before it is killed.
const KILL_AFTER: Duration = Duration::from_secs(60);

/// How often the child is looked at. A deadline is seconds at the least, so
/// this is latency nobody can perceive.
const POLL: Duration = Duration::from_millis(20);

/// The last signal this process caught, for the wait loop to forward.
static CAUGHT: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    CAUGHT.store(signal, Ordering::SeqCst);
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub seconds: u64,
    pub program: String,
    pub args: Vec<String>,
}

/// Parses `<seconds> -- <program> [args..]`.
pub fn parse(args: &[String]) -> Result<Request, String> {
    const USAGE: &str = "usage: zeughaus-runner timeout <seconds> -- <program> [args..]";
    let [seconds, separator, program, rest @ ..] = args else {
        return Err(USAGE.to_owned());
    };
    if separator != "--" {
        return Err(USAGE.to_owned());
    }
    let seconds = seconds
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| format!("{seconds:?} is not a number of seconds above zero"))?;
    Ok(Request {
        seconds,
        program: program.clone(),
        args: rest.to_vec(),
    })
}

/// Runs the request to its end and returns the exit status to leave with.
pub fn run(request: &Request) -> i32 {
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores into an atomic, which is
        // async-signal-safe.
        unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
    }
    let mut child = match Command::new(&request.program)
        .args(&request.args)
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[timeout] cannot run {}: {e}", request.program);
            // The coreutils codes: not found, and found but not runnable.
            return if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
        }
    };
    let group = child.id() as libc::pid_t;
    let deadline = Instant::now() + Duration::from_secs(request.seconds);
    let mut timed_out = false;
    let mut kill_at: Option<Instant> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if timed_out {
                    124
                } else {
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
                };
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[timeout] cannot wait for {}: {e}", request.program);
                return 125;
            }
        }
        let now = Instant::now();
        let caught = CAUGHT.swap(0, Ordering::SeqCst);
        if caught != 0 {
            // SAFETY: a plain signal to the group this process created.
            unsafe { libc::killpg(group, caught) };
            kill_at.get_or_insert(now + KILL_AFTER);
        }
        if !timed_out && now >= deadline {
            timed_out = true;
            // SAFETY: as above.
            unsafe { libc::killpg(group, libc::SIGTERM) };
            kill_at.get_or_insert(now + KILL_AFTER);
        }
        if kill_at.is_some_and(|at| now >= at) {
            // SAFETY: as above.
            unsafe { libc::killpg(group, libc::SIGKILL) };
            kill_at = Some(now + KILL_AFTER);
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_command_line_is_seconds_separator_program_and_arguments() {
        assert_eq!(
            parse(&args(&["5", "--", "sh", "-c", "exit 3"])),
            Ok(Request {
                seconds: 5,
                program: "sh".into(),
                args: args(&["-c", "exit 3"]),
            })
        );
        assert!(parse(&args(&["5", "sh"])).is_err());
        assert!(parse(&args(&["0", "--", "sh"])).is_err());
        assert!(parse(&args(&["soon", "--", "sh"])).is_err());
        assert!(parse(&args(&["5", "--"])).is_err());
    }
}
