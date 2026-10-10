//! `zeughaus-runner ci replay <file> [--raw] [--until <path>]`: the program
//! of a transcript terminal.
//!
//! A transcript is a terminal in the runner's mux that shows one job's
//! output. Its child is this command: it writes the recorded bytes to its own
//! terminal, so the terminal engine renders them the way it rendered the job,
//! and then waits for its terminal to be closed. Internal: the `/ci` service
//! starts it, nobody types it, and `ci` does not list it.
//!
//! `<file>` is a WAL (see [`zeughaus_terminal::wal`]), or with `--raw` the
//! plain `log` of a run from before the WAL. With `--until <path>` a WAL is
//! followed while the run is still writing it, until `<path>` (the run's exit
//! record) exists and nothing more has been written.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use zeughaus_terminal::wal::{self, Follower};

const USAGE: &str = "usage: zeughaus-runner ci replay <file> [--raw] [--until <path>]";

/// How often a followed WAL is looked at again.
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug, PartialEq, Eq)]
struct Options {
    file: PathBuf,
    raw: bool,
    until: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut file = None;
    let mut raw = false;
    let mut until = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--raw" => raw = true,
            "--until" => until = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            _ if file.is_none() => file = Some(PathBuf::from(arg)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let file = file.ok_or(USAGE)?;
    if raw && until.is_some() {
        return Err("--until follows a WAL, which a raw log is not".to_owned());
    }
    Ok(Options { file, raw, until })
}

pub fn run(args: &[String]) -> ExitCode {
    let options = match parse(args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("[ci] {e}");
            return ExitCode::FAILURE;
        }
    };
    raw_stdin();
    let mut out = io::stdout().lock();
    if let Err(e) = replay(&options, &mut out).and_then(|()| out.flush()) {
        // The terminal may be the thing that failed; a message that cannot
        // be written is dropped rather than turned into a panic.
        let _ = writeln!(io::stderr(), "[ci] {}: {e}", options.file.display());
        return ExitCode::FAILURE;
    }
    drop(out);
    wait_for_eof();
    ExitCode::SUCCESS
}

/// Writes what the file holds, and with `--until` what it comes to hold.
fn replay(options: &Options, out: &mut impl Write) -> io::Result<()> {
    match &options.until {
        Some(until) => follow(&options.file, until, POLL, out),
        None => out.write_all(&replay_bytes(&options.file, options.raw)?),
    }
}

/// The bytes a finished recording replays: every record's bytes of a WAL,
/// or a raw log as it is.
fn replay_bytes(path: &Path, raw: bool) -> io::Result<Vec<u8>> {
    if raw {
        std::fs::read(path)
    } else {
        wal::read_bytes(path)
    }
}

/// Writes the WAL's records, those already there and every one that comes,
/// until `until` exists and a read finds nothing more.
fn follow(file: &Path, until: &Path, poll: Duration, out: &mut impl Write) -> io::Result<()> {
    let mut wal = Follower::open(file)?;
    loop {
        // Looked at before the read: the exit record is written after the
        // run's last byte, so a read that starts once it exists sees all of
        // them. Looked at after, a last record could slip in between.
        let ended = until.exists();
        let records = wal.next_records()?;
        for record in &records {
            out.write_all(&record.bytes)?;
        }
        out.flush()?;
        if ended && records.is_empty() {
            return Ok(());
        }
        if !ended {
            std::thread::sleep(poll);
        }
    }
}

/// Puts the terminal on stdin into raw mode, so that nothing typed into a
/// read-only pane is echoed over the transcript and, above all, so that the
/// output is not processed: the `\r\n` of the recording is written as it was
/// recorded, not translated a second time. A stdin that is no terminal is
/// left alone.
fn raw_stdin() {
    // SAFETY: `termios` is plain data that `tcgetattr` fills in before it is
    // read; both calls act on the process's own descriptor 0 and nothing else.
    unsafe {
        let mut mode: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut mode) == 0 {
            libc::cfmakeraw(&mut mode);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &mode);
        }
    }
}

/// Reads and discards stdin until it ends: a transcript lives until its
/// terminal is closed, and nobody types into one.
fn wait_for_eof() {
    let mut stdin = io::stdin().lock();
    let mut sink = [0u8; 256];
    loop {
        match stdin.read(&mut sink) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // A PTY whose master is closed reads as an error, not as EOF.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use zeughaus_terminal::wal::WalWriter;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeughaus-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_wal_replays_as_its_bytes_and_a_raw_log_as_it_is() {
        let dir = scratch("bytes");
        let wal = dir.join("wal");
        let mut writer = WalWriter::open(&wal, None).expect("open the WAL");
        writer.append(1, b"step 1\r\n").expect("append");
        writer
            .append(2, b"\x1b[32mstep 2\x1b[0m\r\n")
            .expect("append");
        assert_eq!(
            replay_bytes(&wal, false).expect("replay the WAL"),
            b"step 1\r\n\x1b[32mstep 2\x1b[0m\r\n"
        );

        let log = dir.join("log");
        std::fs::write(&log, b"before the WAL\nZGHWAL1\n").expect("write the log");
        assert_eq!(
            replay_bytes(&log, true).expect("replay the raw log"),
            b"before the WAL\nZGHWAL1\n"
        );
        // A raw log is no WAL: replaying it without `--raw` says so rather
        // than writing its bytes under a wrong reading.
        assert_eq!(
            replay_bytes(&log, false)
                .expect_err("not a WAL")
                .to_string(),
            format!("not a WAL: {}", log.display())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn following_writes_late_records_and_ends_after_the_exit_record() {
        let dir = scratch("follow");
        let wal = dir.join("wal");
        let exit = dir.join("exit");
        let mut writer = WalWriter::open(&wal, None).expect("open the WAL");
        writer.append(1, b"a").expect("append");

        let exit_file = exit.clone();
        let run = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            writer.append(2, b"b").expect("append");
            std::thread::sleep(Duration::from_millis(40));
            // The last record is written just before the exit record, the
            // order the scheduler's run end has.
            writer.append(3, b"c").expect("append");
            std::fs::write(exit_file, "code=0\n").expect("write the exit record");
        });

        let mut out = Vec::new();
        follow(&wal, &exit, Duration::from_millis(5), &mut out).expect("follow the WAL");
        run.join().expect("the writer thread");
        assert_eq!(out, b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn following_a_file_that_is_no_wal_fails() {
        let dir = scratch("nowal");
        let plain = dir.join("log");
        std::fs::write(&plain, "plain text").expect("write the log");
        let mut out = Vec::new();
        let err = follow(
            &plain,
            &dir.join("exit"),
            Duration::from_millis(5),
            &mut out,
        )
        .expect_err("not a WAL");
        assert_eq!(err.to_string(), format!("not a WAL: {}", plain.display()));
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_arguments_name_a_file_and_two_optional_flags() {
        assert_eq!(
            parse(&args(&["wal"])),
            Ok(Options {
                file: "wal".into(),
                raw: false,
                until: None
            })
        );
        assert_eq!(
            parse(&args(&["--until", "exit", "wal"])),
            Ok(Options {
                file: "wal".into(),
                raw: false,
                until: Some("exit".into())
            })
        );
        assert_eq!(
            parse(&args(&["log", "--raw"])),
            Ok(Options {
                file: "log".into(),
                raw: true,
                until: None
            })
        );
        for refused in [
            args(&[]),
            args(&["--raw"]),
            args(&["wal", "--until"]),
            args(&["wal", "other"]),
        ] {
            assert_eq!(parse(&refused), Err(USAGE.to_owned()), "{refused:?}");
        }
        assert!(
            parse(&args(&["log", "--raw", "--until", "exit"]))
                .expect_err("a raw log cannot be followed")
                .contains("--until")
        );
    }
}
