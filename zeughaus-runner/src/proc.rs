//! Running a helper program and reading its answer.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The stdout of `command`, killed after `wait`.
pub(crate) fn output_within(mut command: Command, wait: Duration) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stdout) = stdout {
            let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        }
        text
    });
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => {
                return reader.join().map_err(|_| "the reader panicked".to_owned());
            }
            Some(status) => return Err(format!("exited with {status}")),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} s", wait.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
