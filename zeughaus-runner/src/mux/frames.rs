//! Reading and writing mux frames on a weida stream.
//!
//! One frame is a [`FrameHeader`] and a body the header sizes. The header is
//! decoded -- and its length bounded by the kind -- before a single body byte
//! is read, so a peer cannot make this process allocate more than the kind
//! allows.

use tokio::io::{AsyncRead, AsyncReadExt};
use weida::OutgoingTransfer;
use zeughaus_mux::{CodecError, FrameHeader, Message};

/// Why a frame could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// The stream ended cleanly between frames.
    Ended,
    /// The stream failed or ended inside a frame.
    Io(std::io::Error),
    Codec(CodecError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Ended => f.write_str("stream ended"),
            ReadError::Io(e) => write!(f, "{e}"),
            ReadError::Codec(e) => write!(f, "{e}"),
        }
    }
}

/// Reads one frame. `Ended` only when the stream finished exactly at a
/// frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(FrameHeader, Message), ReadError> {
    let mut header = [0u8; FrameHeader::LEN];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ReadError::Ended),
        Err(e) => return Err(ReadError::Io(e)),
    }
    let header = FrameHeader::decode(&header).map_err(ReadError::Codec)?;
    let mut body = vec![0u8; header.length as usize];
    reader.read_exact(&mut body).await.map_err(ReadError::Io)?;
    let message = Message::decode(header.kind, &body).map_err(ReadError::Codec)?;
    Ok((header, message))
}

/// Writes one frame. A codec refusal is this side's bug and is reported as
/// such; a transport failure is the peer going away.
pub async fn write_frame(
    writer: &mut OutgoingTransfer,
    message: &Message,
    request_id: u64,
) -> Result<(), String> {
    let bytes = message
        .encode(request_id)
        .map_err(|e| format!("encode {:?}: {e}", message.kind()))?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("write {:?}: {e}", message.kind()))
}
