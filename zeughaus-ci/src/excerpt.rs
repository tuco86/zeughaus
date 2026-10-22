//! What a failed job said last: the excerpt of its log that travels with
//! the result, and the one line of it that names the cause.
//!
//! A run's output is the PTY's bytes as the terminal received them, colours,
//! cursor movement and progress bars included. [`plain_text`] reduces that
//! to the lines a reader would see; the excerpt is the last of them.

use std::path::Path;

use zeughaus_terminal::wal::Record;

/// Lines an excerpt keeps.
pub const EXCERPT_LINES: usize = 40;
/// Bytes read from the end of a log for an excerpt: 40 lines of a build
/// with long paths and colour codes fit many times over.
const TAIL_BYTES: usize = 256 * 1024;
/// What a cause line looks like, in order of preference: a panic or a
/// compiler error says more than the `error: test failed` or
/// `test result: FAILED` cargo prints after it.
const CAUSES: [&str; 4] = ["panicked at", "error[", "error:", "FAILED"];

/// Terminal output as the lines a reader sees: escape sequences removed, a
/// carriage return starting its line over, a backspace moving back one
/// column, so the next character overwrites and a line end keeps what is
/// there, other control characters dropped.
pub fn plain_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut line: Vec<char> = Vec::new();
    // The cursor column, in chars.
    let mut col = 0usize;
    // Set by a carriage return; the next printable character starts the
    // line over, a line feed ends it as it stands (`\r\n`).
    let mut overwrite = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\n' => {
                push_trimmed(&mut out, &line);
                out.push('\n');
                line.clear();
                col = 0;
                overwrite = false;
            }
            '\r' => {
                overwrite = true;
                col = 0;
            }
            '\x08' => col = col.saturating_sub(1),
            '\t' => put(&mut line, &mut col, c),
            c if c.is_control() => {}
            c => {
                if overwrite {
                    line.clear();
                    col = 0;
                    overwrite = false;
                }
                put(&mut line, &mut col, c);
            }
        }
    }
    push_trimmed(&mut out, &line);
    out
}

/// Writes `c` at the cursor and moves it on.
fn put(line: &mut Vec<char>, col: &mut usize, c: char) {
    if *col < line.len() {
        line[*col] = c;
    } else {
        line.push(c);
    }
    *col += 1;
}

/// Appends `line` without its trailing whitespace.
fn push_trimmed(out: &mut String, line: &[char]) {
    let end = line
        .iter()
        .rposition(|c| !c.is_whitespace())
        .map_or(0, |i| i + 1);
    out.extend(&line[..end]);
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

/// The last [`EXCERPT_LINES`] lines of `text`, trailing blank lines dropped.
pub fn excerpt(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut body = lines.as_slice();
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
/// the run finished, so the excerpt is the end of the log. `None` when the
/// output cannot be read.
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

/// The lines of a run's output with the time each began, from the WAL
/// records that hold them: the text as [`plain_text`] shows it, and the time
/// (microseconds since the UNIX epoch) of the record in which the line's
/// first byte arrived. A step that prints its name and finishes later is
/// thus one line at the moment it began, however late the rest of it comes.
/// Lines with no text are left out.
pub fn timed_lines(records: &[Record]) -> Vec<(u64, String)> {
    let mut lines = Vec::new();
    // The line in progress: a record may end inside a line, or inside a
    // character, so its bytes wait for the next one.
    let mut raw: Vec<u8> = Vec::new();
    let mut at = 0;
    for record in records {
        let mut pieces = record.bytes.split(|&b| b == b'\n').peekable();
        while let Some(piece) = pieces.next() {
            if raw.is_empty() {
                at = record.at_micros;
            }
            raw.extend_from_slice(piece);
            // Every piece but the last ends at a line feed.
            if pieces.peek().is_some() {
                push_timed(&mut lines, at, &raw);
                raw.clear();
            }
        }
    }
    push_timed(&mut lines, at, &raw);
    lines
}

fn push_timed(lines: &mut Vec<(u64, String)>, at: u64, raw: &[u8]) {
    let text = plain_text(raw);
    if !text.is_empty() {
        lines.push((at, text));
    }
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

    /// ConPTY ends every row it wraps with a backspace: the character before
    /// it stays.
    #[test]
    fn a_backspace_moves_back_without_erasing() {
        assert_eq!(
            plain_text(b"until_its\x08\r\n_socket"),
            "until_its\n_socket"
        );
        assert_eq!(plain_text(b"abc\x08\x08X"), "aXc");
    }

    #[test]
    fn excerpt_keeps_the_last_forty_lines_and_drops_trailing_blank_ones() {
        let mut text: String = (0..60).map(|i| format!("line {i}\n")).collect();
        text.push_str("\n\n");
        let excerpt = excerpt(&text);
        let lines: Vec<&str> = excerpt.lines().collect();
        assert_eq!(lines.len(), EXCERPT_LINES);
        assert_eq!(lines.first(), Some(&"line 20"));
        assert_eq!(lines.last(), Some(&"line 59"));
    }

    /// A line the launcher printed itself (a failed checkout) is the end of
    /// the log like any other.
    #[test]
    fn a_launcher_line_at_the_end_stays_in_the_excerpt() {
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

    fn record(at_micros: u64, bytes: &[u8]) -> Record {
        Record {
            at_micros,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn a_line_has_the_time_of_the_record_its_first_byte_came_in() {
        // "bc" begins in the first record and ends in the second.
        let records = [record(1, b"a\nb"), record(5, b"c\n")];
        assert_eq!(
            timed_lines(&records),
            [(1, "a".to_owned()), (1, "bc".to_owned())]
        );
    }

    #[test]
    fn timed_lines_show_what_a_reader_sees_and_leave_out_what_is_empty() {
        let records = [
            // Colours, a progress bar, a blank line, a line of spaces, and
            // a character cut between two records.
            record(
                10,
                b"\x1b[32mone\x1b[0m\r\n 10%\r100%\r\n\r\n   \r\ncaf\xc3",
            ),
            record(20, b"\xa9\r\n"),
            // A record without bytes changes nothing; the last line needs
            // no line feed.
            record(30, b""),
            record(40, b"tail"),
        ];
        assert_eq!(
            timed_lines(&records),
            [
                (10, "one".to_owned()),
                (10, "100%".to_owned()),
                (10, "caf\u{e9}".to_owned()),
                (40, "tail".to_owned()),
            ]
        );
        assert!(timed_lines(&[]).is_empty());
    }
}
