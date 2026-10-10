//! The frames between a session and its shim, and the spec a shim starts
//! from.
//!
//! A frame is a `u32` little-endian body length, a `u8` kind, and a postcard
//! body. The length is checked against [`MAX_FRAME`] before anything is
//! allocated, so a corrupt or hostile peer costs a refused connection, not
//! memory. Output travels borrowed from the reader's buffer: a chunk the
//! shim reads from its PTY is framed without a copy, and a chunk the session
//! receives is parsed out of the frame buffer it arrived in.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped on any change to a frame's layout. A session and a shim of
/// different versions refuse each other at `Hello`: a runner rebuilt with a
/// new protocol cannot adopt the shims of the old one.
pub const PROTOCOL: u16 = 1;

/// Largest frame body either side accepts.
pub const MAX_FRAME: usize = 1 << 20;

/// Output kept by a shim for the next session that attaches: what that
/// session's screen is rebuilt from. The oldest bytes are dropped first.
pub const REPLAY_BYTES: usize = 4 << 20;

/// Most output bytes in one frame, the replay's included. Matches the
/// session's PTY read size, so a live chunk is always one frame.
pub const OUTPUT_CHUNK: usize = 64 * 1024;

/// What a shim starts: the fields of a [`crate::Profile`], the grid it opens
/// the PTY with, and where it tees the child's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimSpec {
    pub label: String,
    pub program: Option<PathBuf>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub scrollback_rows: usize,
    pub cols: u16,
    pub rows: u16,
    /// The WAL the shim tees the child's output into (see [`crate::wal`]);
    /// appended to, never truncated: a job's WAL is created by its host
    /// before the run starts.
    pub wal: Option<PathBuf>,
    /// Size at which that WAL rotates into `<wal>.1`; `None` never rotates.
    pub wal_cap: Option<u64>,
}

/// How the shim's child ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimExit {
    pub code: Option<u32>,
    pub signal: Option<String>,
    /// A session asked for the kill; whatever status it produced, this was
    /// deliberate.
    pub killed: bool,
}

/// The shim's answer to `Hello`.
///
/// The replay does not fit a frame (it is up to [`REPLAY_BYTES`]), so it
/// follows as `replay_bytes` bytes of `Output` frames; everything after
/// those is live output. The shim sends all of it under the lock its reader
/// takes, so replay and live output neither overlap nor leave a gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u16,
    pub child_pid: u32,
    pub replay_bytes: u64,
    pub exit: Option<ShimExit>,
}

/// Session to shim. `Hello` is always first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToShim<'a> {
    Hello {
        version: u16,
    },
    Input(&'a [u8]),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Wakes the foreground process group with SIGWINCH, so a full-screen
    /// program repaints what a replay may have cut off.
    Redraw,
    /// Kill the child, remove the shim's directory, end the shim.
    Close,
}

/// Shim to session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromShim<'a> {
    Welcome(Welcome),
    Output(&'a [u8]),
    Exited(ShimExit),
}

const HELLO: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const REDRAW: u8 = 4;
const CLOSE: u8 = 5;
const WELCOME: u8 = 16;
const OUTPUT: u8 = 17;
const EXITED: u8 = 18;

/// One whole frame, ready for a single `write_all`: two writers sharing a
/// socket must never interleave halves of their frames.
fn frame(kind: u8, body: &impl Serialize) -> Vec<u8> {
    let mut out = vec![0u8; 5];
    out[4] = kind;
    // Serializing into a `Vec` cannot fail for these types.
    let mut out = postcard::to_extend(body, out).expect("postcard into a Vec");
    let len = (out.len() - 5) as u32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    out
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn decode<'a, T: Deserialize<'a>>(body: &'a [u8]) -> io::Result<T> {
    postcard::from_bytes(body).map_err(invalid)
}

impl ToShim<'_> {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToShim::Hello { version } => frame(HELLO, version),
            ToShim::Input(bytes) => frame(INPUT, bytes),
            ToShim::Resize { cols, rows } => frame(RESIZE, &(cols, rows)),
            ToShim::Redraw => frame(REDRAW, &()),
            ToShim::Close => frame(CLOSE, &()),
        }
    }

    pub fn decode(kind: u8, body: &[u8]) -> io::Result<ToShim<'_>> {
        Ok(match kind {
            HELLO => ToShim::Hello {
                version: decode(body)?,
            },
            INPUT => ToShim::Input(decode(body)?),
            RESIZE => {
                let (cols, rows) = decode(body)?;
                ToShim::Resize { cols, rows }
            }
            REDRAW => ToShim::Redraw,
            CLOSE => ToShim::Close,
            other => return Err(invalid(format!("frame kind {other} to a shim"))),
        })
    }
}

impl FromShim<'_> {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            FromShim::Welcome(welcome) => frame(WELCOME, welcome),
            FromShim::Output(bytes) => frame(OUTPUT, bytes),
            FromShim::Exited(exit) => frame(EXITED, exit),
        }
    }

    pub fn decode(kind: u8, body: &[u8]) -> io::Result<FromShim<'_>> {
        Ok(match kind {
            WELCOME => FromShim::Welcome(decode(body)?),
            OUTPUT => FromShim::Output(decode(body)?),
            EXITED => FromShim::Exited(decode(body)?),
            other => return Err(invalid(format!("frame kind {other} from a shim"))),
        })
    }
}

/// Reads one frame into `buf`. `Ok(None)` is a clean end between frames;
/// an end inside one is an error.
pub fn read_frame<'b>(
    reader: &mut impl Read,
    buf: &'b mut Vec<u8>,
) -> io::Result<Option<(u8, &'b [u8])>> {
    let mut header = [0u8; 5];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes refused")));
    }
    buf.resize(len, 0);
    reader.read_exact(buf)?;
    Ok(Some((header[4], buf.as_slice())))
}

/// Writes one already encoded frame.
pub fn write_frame(writer: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    writer.write_all(frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_oversized_ones_are_refused() {
        let welcome = Welcome {
            version: PROTOCOL,
            child_pid: 42,
            replay_bytes: 7,
            exit: Some(ShimExit {
                code: Some(3),
                signal: None,
                killed: false,
            }),
        };
        let mut wire = FromShim::Welcome(welcome.clone()).encode();
        wire.extend(FromShim::Output(b"hello").encode());
        wire.extend(ToShim::Resize { cols: 80, rows: 24 }.encode());
        let mut reader = wire.as_slice();
        let mut buf = Vec::new();

        let (kind, body) = read_frame(&mut reader, &mut buf).unwrap().unwrap();
        assert_eq!(
            FromShim::decode(kind, body).unwrap(),
            FromShim::Welcome(welcome)
        );
        let (kind, body) = read_frame(&mut reader, &mut buf).unwrap().unwrap();
        assert_eq!(
            FromShim::decode(kind, body).unwrap(),
            FromShim::Output(b"hello")
        );
        let (kind, body) = read_frame(&mut reader, &mut buf).unwrap().unwrap();
        assert_eq!(
            ToShim::decode(kind, body).unwrap(),
            ToShim::Resize { cols: 80, rows: 24 }
        );
        assert!(read_frame(&mut reader, &mut buf).unwrap().is_none());

        let mut huge = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        huge.push(OUTPUT);
        let error = read_frame(&mut huge.as_slice(), &mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // Refused before the body was allocated.
        assert!(buf.capacity() < MAX_FRAME);
    }
}
