//! What a failed job said last: the excerpt of its log that travels with
//! the result, and the one line of it that names the cause.
//!
//! A run's output is the PTY's bytes as the terminal received them, colours,
//! cursor movement and progress bars included. [`plain_text`] reduces that
//! to the lines a reader would see; the excerpt is the tail before the
//! launcher's `[zeughaus-ci] <job> exited <code>` marker, so the shell that
//! follows a failure never ends up in it.

use std::path::Path;

/// Lines an excerpt keeps.
pub const EXCERPT_LINES: usize = 40;
/// Bytes read from the end of a log for an excerpt: 40 lines of a build
/// with long paths and colour codes fit many times over.
const TAIL_BYTES: usize = 256 * 1024;
/// The prefix of every line the launcher prints itself.
const MARKER: &str = "[zeughaus-ci] ";
/// What a cause line looks like, in order of preference: a panic or a
/// compiler error says more than the `error: test failed` or
/// `test result: FAILED` cargo prints after it.
const CAUSES: [&str; 4] = ["panicked at", "error[", "error:", "FAILED"];

/// Terminal output as the lines a reader sees: escape sequences removed, a
/// carriage return starting its line over, a backspace taking back a
/// character, other control characters dropped.
pub fn plain_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut line = String::new();
    // Set by a carriage return; the next printable character starts the
    // line over, a line feed ends it as it stands (`\r\n`).
    let mut overwrite = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\n' => {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
                overwrite = false;
            }
            '\r' => overwrite = true,
            '\x08' => {
                line.pop();
            }
            '\t' => line.push(c),
            c if c.is_control() => {}
            c => {
                if overwrite {
                    line.clear();
                    overwrite = false;
                }
                line.push(c);
            }
        }
    }
    out.push_str(line.trim_end());
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC, DCS, SOS, PM, APC: a string up to BEL or ST.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        }
        // Charset designations and the like carry one more character.
        Some('(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%' | ' ') => {
            chars.next();
        }
        _ => {}
    }
}

/// Parameters and intermediates up to the final byte.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('\x40'..='\x7e').contains(&c) {
            break;
        }
    }
}

fn is_marker(line: &str) -> bool {
    line.trim_start().starts_with(MARKER) && line.contains(" exited ")
}

/// The last [`EXCERPT_LINES`] lines before the last exit marker, trailing
/// blank lines dropped; the end of the text when there is no marker.
pub fn excerpt(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .iter()
        .rposition(|l| is_marker(l))
        .unwrap_or(lines.len());
    let mut body = &lines[..end];
    while let Some((last, rest)) = body.split_last()
        && last.trim().is_empty()
    {
        body = rest;
    }
    body[body.len().saturating_sub(EXCERPT_LINES)..].join("\n")
}

/// The first line of `excerpt` that looks like a cause, by the order of
/// [`CAUSES`]. A line ending in `:` (Rust's `panicked at <file>:<line>:`)
/// takes the next line along, which holds the message.
pub fn cause(excerpt: &str) -> Option<String> {
    let lines: Vec<&str> = excerpt.lines().map(str::trim).collect();
    CAUSES.iter().find_map(|pattern| {
        let i = lines.iter().position(|l| l.contains(pattern))?;
        let mut text = lines[i].to_owned();
        if text.ends_with(':')
            && let Some(next) = lines.get(i + 1).filter(|n| !n.is_empty())
        {
            text.push(' ');
            text.push_str(next);
        }
        Some(text)
    })
}

/// The excerpt of the failed run whose transcript is in `dir`, also written
/// to `<dir>/excerpt`. The transcript is complete when the runner reports
/// the run finished, so a missing exit marker means none was printed (a
/// launcher that failed before the job) and the excerpt is the end of the
/// log. `None` when the output cannot be read.
pub fn extract(dir: &Path) -> Option<String> {
    let text = plain_text(&run_output_tail(dir, TAIL_BYTES).ok()?);
    let excerpt = excerpt(&text);
    if let Err(e) = crate::files::write_atomic(&dir.join("excerpt"), excerpt.as_bytes()) {
        eprintln!("[ci] {}: {e}", dir.display());
    }
    Some(excerpt)
}

/// A whole run's output as plain text, for `ci log`.
pub fn read_log(dir: &Path) -> Result<String, String> {
    zeughaus_terminal::wal::read_bytes(&dir.join("wal"))
        .map(|bytes| plain_text(&bytes))
        .map_err(|e| format!("cannot read the output of {}: {e}", dir.display()))
}

/// The end of a run's output as plain text: what the last `max` bytes of the
/// transcript say, from the first line that starts inside them. Unlike
/// [`read_log`] it never holds more than `max` bytes of a long log.
pub fn read_log_tail(dir: &Path, max: usize) -> Result<String, String> {
    run_output_tail(dir, max)
        .map(|bytes| plain_text(&bytes))
        .map_err(|e| format!("cannot read the output of {}: {e}", dir.display()))
}

/// The last `max` bytes of the transcript, from the first line that starts
/// inside them: a cut through a character or an escape sequence would
/// otherwise open the text.
fn run_output_tail(dir: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = zeughaus_terminal::wal::tail_bytes(&dir.join("wal"), max)?;
    if bytes.len() >= max
        && let Some(newline) = bytes.iter().position(|&b| b == b'\n')
    {
        bytes.drain(..=newline);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_drops_escapes_and_applies_carriage_returns() {
        let raw = b"\x1b[1m\x1b[32m   Compiling\x1b[0m zeughaus\r\n\
            \x1b]0;title\x07 10%\r 50%\r100%\r\n\
            ab\x08c\x1b(B\x1b[?25l\r\r\n\
            \x1b]8;;file:///x\x1b\\link\x1b]8;;\x1b\\ \t end";
        assert_eq!(
            plain_text(raw),
            "   Compiling zeughaus\n100%\nac\nlink \t end"
        );
    }

    #[test]
    fn excerpt_ends_before_the_last_marker_and_keeps_forty_lines() {
        let mut text: String = (0..60).map(|i| format!("line {i}\n")).collect();
        text.push_str("\n\n[zeughaus-ci] windows exited 101; a shell follows\nPS W:\\> dir\n");
        let excerpt = excerpt(&text);
        let lines: Vec<&str> = excerpt.lines().collect();
        assert_eq!(lines.len(), EXCERPT_LINES);
        assert_eq!(lines.first(), Some(&"line 20"));
        assert_eq!(lines.last(), Some(&"line 59"));
    }

    #[test]
    fn excerpt_without_a_marker_is_the_end_of_the_log() {
        let excerpt = excerpt("one\n[zeughaus-ci] checkout failed\n\n");
        assert_eq!(excerpt, "one\n[zeughaus-ci] checkout failed");
    }

    #[test]
    fn cause_prefers_the_panic_and_joins_its_message() {
        let excerpt = "test experiments::eight_in_time ... FAILED\n\
            thread 'eight_in_time' panicked at crates/infer/tests/experiments.rs:208:5:\n\
            assertion `left == right` failed\n\
            error: test failed, to rerun pass `-p griasdi-infer --test experiments`";
        assert_eq!(
            cause(excerpt).as_deref(),
            Some(
                "thread 'eight_in_time' panicked at crates/infer/tests/experiments.rs:208:5: \
                 assertion `left == right` failed"
            )
        );
    }

    #[test]
    fn cause_prefers_the_compiler_error_over_the_summary() {
        let excerpt = "error[E0425]: cannot find value `x` in this scope\n\
            error: could not compile `zeughaus` (lib) due to 1 previous error";
        assert_eq!(
            cause(excerpt).as_deref(),
            Some("error[E0425]: cannot find value `x` in this scope")
        );
        assert_eq!(cause("all good\nexit 3"), None);
    }
}
