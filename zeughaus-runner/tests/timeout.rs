//! The `timeout` subcommand through the built binary, as a host job's
//! `inner.sh` (or a Windows runner's `launch.ps1`) runs it.

/// Tests that touch the filesystem, network, processes or real time: the
/// integration tier (`.config/nextest.toml`).
mod io {
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn runner() -> Command {
        Command::new(env!("CARGO_BIN_EXE_zeughaus-runner"))
    }

    /// `script` run by this host's shell: `sh -c` on unix, PowerShell on
    /// Windows, whose jobs are `.ps1` files.
    fn shell(script: &str) -> Vec<&str> {
        if cfg!(windows) {
            vec!["powershell", "-NoProfile", "-Command", script]
        } else {
            vec!["sh", "-c", script]
        }
    }

    #[test]
    fn a_program_past_its_deadline_is_ended_and_exits_124() {
        let sleep = if cfg!(windows) {
            "Start-Sleep 5"
        } else {
            "sleep 5"
        };
        let started = Instant::now();
        let status = runner()
            .args(["timeout", "1", "--"])
            .args(shell(sleep))
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
            .args(["timeout", "5", "--"])
            .args(shell("exit 3"))
            .status()
            .expect("run the runner");
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn a_flag_of_the_runner_after_the_separator_belongs_to_the_program() {
        let program: &[&str] = if cfg!(windows) {
            &["cmd", "/c", "echo"]
        } else {
            &["sh", "-c", "printf '%s\\n' \"$0 $1\""]
        };
        let output = runner()
            .args(["timeout", "5", "--"])
            .args(program)
            .args(["--state-dir", "x"])
            .output()
            .expect("run the runner");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim_end(),
            "--state-dir x"
        );
    }

    #[test]
    fn a_program_that_cannot_run_exits_127() {
        let status = runner()
            .args(["timeout", "5", "--", "/nonexistent/zeughaus-program"])
            .status()
            .expect("run the runner");
        assert_eq!(status.code(), Some(127));
    }
}
