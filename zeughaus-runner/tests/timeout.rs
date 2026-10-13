//! The `timeout` subcommand through the built binary, as a host job's
//! `inner.sh` runs it.

use std::process::Command;
use std::time::{Duration, Instant};

fn runner() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zeughaus-runner"))
}

#[test]
fn a_program_past_its_deadline_is_ended_and_exits_124() {
    let started = Instant::now();
    let status = runner()
        .args(["timeout", "1", "--", "sh", "-c", "sleep 5"])
        .status()
        .expect("run the runner");
    assert_eq!(status.code(), Some(124));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_program_that_ends_in_time_keeps_its_exit_code() {
    let status = runner()
        .args(["timeout", "5", "--", "sh", "-c", "exit 3"])
        .status()
        .expect("run the runner");
    assert_eq!(status.code(), Some(3));
}

#[test]
fn a_flag_of_the_runner_after_the_separator_belongs_to_the_program() {
    let output = runner()
        .args([
            "timeout",
            "5",
            "--",
            "sh",
            "-c",
            "printf %s \"$0 $1\"",
            "--state-dir",
            "x",
        ])
        .output()
        .expect("run the runner");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "--state-dir x");
}

#[test]
fn a_program_that_cannot_run_exits_127() {
    let status = runner()
        .args(["timeout", "5", "--", "/nonexistent/zeughaus-program"])
        .status()
        .expect("run the runner");
    assert_eq!(status.code(), Some(127));
}
