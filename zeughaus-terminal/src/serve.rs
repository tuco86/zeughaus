//! The read-only half of serving a terminal over mux frames: the head, the
//! deltas that follow it, and row fetches. Who may type, and the control
//! leases that decide it, belong to the caller.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::AsyncWrite;
use zeughaus_mux::message::ErrorCode;
use zeughaus_mux::{Message, RowFetch, RowPage, TerminalAttach, TerminalAttached, WireError};

use crate::Session;
use crate::frames::write_frame;

/// Scrollback rows sent with a head, above the visible ones: enough that a
/// wheel notch or two needs no fetch, bounded so an attach stays small.
pub const HEAD_ROWS_ABOVE: usize = 128;

/// The render cadence a subscriber coalesces at during a burst: a delta at
/// most this often per terminal per client, whatever the child prints. A
/// change after quiet goes out at once -- a keystroke's echo is not a burst,
/// and waiting for company would put this whole window on every keystroke.
const COALESCE: Duration = Duration::from_millis(12);

/// Sends `TerminalAttached` and then a delta (or, after a screen switch, a
/// head) per change of `session`, until the stream fails, the session's
/// change channel closes, or `gone` says so after a send.
///
/// `acked` is the highest input serial the caller has handled for this
/// client; every delta carries it. `on_attached` runs once the attach reply
/// is written, so whatever it changes (the attach's own resize) reaches the
/// client as a delta behind the head it was sent. `gone` is asked after every
/// delta: `true` ends the stream (the pane was closed and the final delta
/// carried the exit).
///
/// The caller owns cancellation: dropping this future between frames is
/// safe, and the exchange ends with it. An `Err` is a failed write, that is
/// the peer going away.
pub async fn stream_terminal<W: AsyncWrite + Unpin>(
    session: &Session,
    attach: &TerminalAttach,
    acked: &AtomicU64,
    reply: &mut W,
    on_attached: impl FnOnce(),
    gone: impl Fn() -> bool,
) -> Result<(), String> {
    let mut changes = session.changes();
    let current = *changes.borrow_and_update();
    let epoch = session.epoch();
    let mut head = match attach.known {
        Some(known) if known == (epoch, current) => None,
        _ => Some(session.head(HEAD_ROWS_ABOVE)),
    };
    // Serials are per client and per exchange: a fresh attach starts
    // its count at zero, whatever the engine has applied for others.
    if let Some(head) = head.as_mut() {
        head.input_serial_ack = 0;
    }
    let mut sent_seq = head.as_ref().map_or(current, |h| h.seq);
    let mut sent_epoch = head.as_ref().map_or(epoch, |h| h.epoch);
    write_frame(
        reply,
        &Message::TerminalAttached(TerminalAttached { head }),
        0,
    )
    .await?;
    on_attached();

    // Every row written since the last delta travels, wherever the screen
    // has scrolled it to, so what a client is scrolled to needs no
    // bookkeeping here; `Viewport` is accepted and changes nothing.
    let mut last_sent = Instant::now() - COALESCE;
    loop {
        if changes.changed().await.is_err() {
            return Ok(());
        }
        // Pace, do not delay: a change within the cadence of the last delta
        // waits for the rest of it, a change after quiet is sent as it is.
        let due = last_sent + COALESCE;
        let now = Instant::now();
        if due > now {
            tokio::time::sleep(due - now).await;
        }
        changes.borrow_and_update();
        last_sent = Instant::now();
        let delta = session.delta_since(sent_seq);
        // A screen switch renumbers the rows: what the client holds is of the
        // other screen, and only a head replaces it.
        let message = if delta.epoch == sent_epoch {
            let mut delta = delta;
            delta.input_serial_ack = acked.load(Ordering::SeqCst);
            sent_seq = delta.to_seq;
            Message::TerminalDelta(delta)
        } else {
            let mut head = session.head(HEAD_ROWS_ABOVE);
            head.input_serial_ack = acked.load(Ordering::SeqCst);
            sent_seq = head.seq;
            sent_epoch = head.epoch;
            Message::TerminalHead(head)
        };
        write_frame(reply, &message, 0).await?;
        if gone() {
            return Ok(());
        }
    }
}

/// The reply to a row fetch: a `RowPage`, or the error to send in its place
/// (`Stale` for an epoch the session no longer has).
pub fn fetch_rows(session: &Session, fetch: &RowFetch) -> Message {
    let Some((seq, first_retained, rows)) = session.rows(fetch.epoch, fetch.range) else {
        return Message::Error(WireError {
            code: ErrorCode::Stale,
            message: "unknown epoch".to_owned(),
        });
    };
    Message::RowPage(RowPage {
        terminal: fetch.terminal,
        epoch: fetch.epoch,
        generation: fetch.generation,
        first_retained,
        seq,
        rows,
    })
}
