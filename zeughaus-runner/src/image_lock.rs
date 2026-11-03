//! `zeughaus-runner image-lock <file> -- <program> [args..]`: runs the
//! program while holding a shared `flock` on `<file>`.
//!
//! A container job's launcher builds its image under it, and the runner's
//! image cleanup takes the same file exclusively
//! (`enlist::cleanup::try_lock_images`), so a cleanup never collects the
//! blobs of an image that is being loaded.

use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::process::Command;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub file: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

/// Parses `<file> -- <program> [args..]`.
pub fn parse(args: &[String]) -> Result<Request, String> {
    const USAGE: &str = "usage: zeughaus-runner image-lock <file> -- <program> [args..]";
    let [file, separator, program, rest @ ..] = args else {
        return Err(USAGE.to_owned());
    };
    if separator != "--" {
        return Err(USAGE.to_owned());
    }
    Ok(Request {
        file: PathBuf::from(file),
        program: program.clone(),
        args: rest.to_vec(),
    })
}

/// Runs the program under the shared lock and returns the exit status to
/// leave with: the program's, 125 when the lock cannot be taken, and the
/// coreutils codes 127 and 126 when the program does not start.
///
/// There is no signal forwarding: a killed run's terminal ends the whole
/// process group, the child included.
pub fn run(request: &Request) -> i32 {
    let path = &request.file;
    let cannot_lock = |e: &dyn std::fmt::Display| {
        eprintln!("[zeughaus-ci] cannot lock {}: {e}", path.display());
        125
    };
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) => return cannot_lock(&e),
    };
    match file.try_lock_shared() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            eprintln!("[zeughaus-ci] image build waits for the runner's image cleanup");
            if let Err(e) = file.lock_shared() {
                return cannot_lock(&e);
            }
        }
        Err(TryLockError::Error(e)) => return cannot_lock(&e),
    }
    let status = match Command::new(&request.program).args(&request.args).status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("[zeughaus-ci] cannot run {}: {e}", request.program);
            return if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
        }
    };
    // The lock lives exactly as long as the build: `file` drops only now.
    drop(file);
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_command_line_is_file_separator_program_and_arguments() {
        assert_eq!(
            parse(&args(&[
                "/s/ci/images.lock",
                "--",
                "container",
                "build",
                "-t",
                "x"
            ])),
            Ok(Request {
                file: PathBuf::from("/s/ci/images.lock"),
                program: "container".into(),
                args: args(&["build", "-t", "x"]),
            })
        );
        assert!(parse(&args(&["/f", "container"])).is_err());
        assert!(parse(&args(&["/f", "--"])).is_err());
        assert!(parse(&args(&[])).is_err());
    }
}
