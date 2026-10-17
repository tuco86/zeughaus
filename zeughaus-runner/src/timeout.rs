//! `zeughaus-runner timeout <seconds> -- <program> [args..]`: a program with a
//! deadline.
//!
//! A host CI job runs under this because macOS has no coreutils `timeout`
//! and Windows none at all. On unix it follows the coreutils contract the
//! container jobs get: the child runs in a process group of its own, the
//! group gets `SIGTERM` at the deadline and `SIGKILL` a minute later, and the
//! exit status is 124 when the deadline ended the run, otherwise the child's
//! code (`128 + signal` for a signal). A `SIGHUP`, `SIGINT` or `SIGTERM` sent
//! to this process goes to the child's group too: the group is not the
//! terminal's foreground group, so nothing else would deliver it.
//!
//! On Windows the child runs in a job object, which has no gentle stop: the
//! deadline terminates every process in it with 124. The job also ends with
//! this process and with the launcher that started it, so a run that is
//! killed takes its build with it.

use std::process::Command;
use std::time::{Duration, Instant};

/// How often the child is looked at. A deadline is seconds at the least, so
/// this is latency nobody can perceive.
const POLL: Duration = Duration::from_millis(20);

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

/// The coreutils codes for a program that did not start: not found, and
/// found but not runnable.
fn spawn_failed(program: &str, e: &std::io::Error) -> i32 {
    eprintln!("[timeout] cannot run {program}: {e}");
    if e.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    }
}

#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicI32, Ordering};

    /// The last signal this process caught, for the wait loop to forward.
    static CAUGHT: AtomicI32 = AtomicI32::new(0);

    extern "C" fn on_signal(signal: libc::c_int) {
        CAUGHT.store(signal, Ordering::SeqCst);
    }

    pub fn install() {
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            // SAFETY: the handler only stores into an atomic, which is
            // async-signal-safe.
            unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
        }
    }

    pub fn take() -> libc::c_int {
        CAUGHT.swap(0, Ordering::SeqCst)
    }
}

/// Runs the request to its end and returns the exit status to leave with.
#[cfg(unix)]
pub fn run(request: &Request) -> i32 {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    /// How long a group has to end after `SIGTERM` before it is killed.
    const KILL_AFTER: Duration = Duration::from_secs(60);

    signals::install();
    let mut child = match Command::new(&request.program)
        .args(&request.args)
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return spawn_failed(&request.program, &e),
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
        let caught = signals::take();
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

/// Runs the request to its end and returns the exit status to leave with.
///
/// The child is put into the job right after it started, so a process it
/// spawns in its first instants could escape the job; a shell starting a
/// build does not do that. A closed terminal tells this process nothing, so
/// the end of the launcher that started it (a killed run) ends the job too.
#[cfg(windows)]
pub fn run(request: &Request) -> i32 {
    use std::os::windows::io::AsRawHandle;

    use crate::winjob::{self, Job};

    let job = match Job::kill_on_close() {
        Ok(job) => job,
        Err(e) => {
            eprintln!("[timeout] cannot create a job object: {e}");
            return 125;
        }
    };
    let launcher = winjob::parent();
    let mut child = match Command::new(&request.program).args(&request.args).spawn() {
        Ok(child) => child,
        Err(e) => return spawn_failed(&request.program, &e),
    };
    if let Err(e) = job.assign(child.as_raw_handle()) {
        eprintln!(
            "[timeout] cannot put {} into the job object: {e}",
            request.program
        );
        let _ = child.kill();
        let _ = child.wait();
        return 125;
    }
    let deadline = Instant::now() + Duration::from_secs(request.seconds);
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // An NTSTATUS such as 0xC0000005 does not fit an exit code
                // of this process; it stays a failure.
                return if timed_out {
                    124
                } else {
                    status
                        .code()
                        .filter(|code| (0..=255).contains(code))
                        .unwrap_or(1)
                };
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[timeout] cannot wait for {}: {e}", request.program);
                return 125;
            }
        }
        if !timed_out && Instant::now() >= deadline {
            timed_out = true;
            job.terminate(124);
        }
        if launcher.as_ref().is_some_and(winjob::has_exited) {
            job.terminate(1);
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
