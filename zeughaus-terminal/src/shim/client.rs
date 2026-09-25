//! A session's end of a shim connection.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::proto::{FromShim, PROTOCOL, REPLAY_BYTES, ToShim, Welcome, read_frame, write_frame};
use crate::SpawnError;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// The sending half, shared by the terminal's writer and the session's
/// control calls. Each frame is one `write_all` under the lock, so their
/// frames never interleave.
#[derive(Clone)]
pub struct ShimSender(Arc<Mutex<UnixStream>>);

impl ShimSender {
    pub fn send(&self, frame: &ToShim<'_>) -> io::Result<()> {
        let bytes = frame.encode();
        let mut stream = self.0.lock().unwrap_or_else(|e| e.into_inner());
        write_frame(&mut *stream, &bytes)
    }
}

/// One connection to a shim, after the handshake.
pub struct ShimConn {
    sender: ShimSender,
    reader: UnixStream,
    buf: Vec<u8>,
}

impl ShimConn {
    /// Connects to the shim serving `dir`, says hello, and collects the
    /// replay that precedes the live output.
    pub fn connect(dir: &Path) -> Result<(ShimConn, Welcome, Vec<u8>), SpawnError> {
        let failed = |e: io::Error| SpawnError::Pty(format!("shim {}: {e}", dir.display()));
        let stream = UnixStream::connect(dir.join("sock")).map_err(failed)?;
        let reader = stream.try_clone().map_err(failed)?;
        let mut conn = ShimConn {
            sender: ShimSender(Arc::new(Mutex::new(stream))),
            reader,
            buf: Vec::new(),
        };
        conn.send(&ToShim::Hello { version: PROTOCOL })
            .map_err(failed)?;
        let welcome = match conn.recv().map_err(failed)? {
            Some(FromShim::Welcome(welcome)) => welcome,
            Some(_) => return Err(failed(invalid("first frame is not Welcome"))),
            None => return Err(failed(io::ErrorKind::UnexpectedEof.into())),
        };
        if welcome.version != PROTOCOL {
            return Err(SpawnError::Pty(format!(
                "shim protocol {}",
                welcome.version
            )));
        }
        let expected = usize::try_from(welcome.replay_bytes)
            .ok()
            .filter(|&n| n <= REPLAY_BYTES)
            .ok_or_else(|| failed(invalid("replay beyond the ring")))?;
        let mut replay = Vec::with_capacity(expected);
        while replay.len() < expected {
            match conn.recv().map_err(failed)? {
                Some(FromShim::Output(bytes)) => replay.extend_from_slice(bytes),
                Some(_) => return Err(failed(invalid("replay interrupted"))),
                None => return Err(failed(io::ErrorKind::UnexpectedEof.into())),
            }
        }
        Ok((conn, welcome, replay))
    }

    pub fn sender(&self) -> ShimSender {
        self.sender.clone()
    }

    pub fn send(&self, frame: &ToShim<'_>) -> io::Result<()> {
        self.sender.send(frame)
    }

    /// The next frame, borrowed from this connection's buffer. `Ok(None)`
    /// is the shim closing the connection.
    pub fn recv(&mut self) -> io::Result<Option<FromShim<'_>>> {
        match read_frame(&mut self.reader, &mut self.buf)? {
            Some((kind, body)) => FromShim::decode(kind, body).map(Some),
            None => Ok(None),
        }
    }
}

/// Ends the shim serving `dir`, or removes the directory if no shim answers
/// there any more.
pub fn close_dir(dir: &Path) {
    match ShimConn::connect(dir) {
        Ok((conn, _, _)) => {
            let _ = conn.send(&ToShim::Close);
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
