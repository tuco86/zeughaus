//! The editor's client for a runner's graph: the document it holds, and the
//! edits that change it.
//!
//! One long-lived task per runner endpoint, on the one [`GRAPH_PATH`]. It
//! attaches, reports the whole document, then every change the runner
//! applies, and writes the edits the editor sends. The runner is the only
//! writer of the document: an edit that is applied comes back as a change,
//! one that is not comes back as a refusal.
//!
//! Nothing here redials. The first dial is [`first_dial`]'s and every later
//! one is weida's; what a redial does not restore is the exchange, so a
//! `Lost` is reported upwards and the next attach starts over from a fresh
//! document. A `GaveUp` ends the task: a runner that restarted has a fresh
//! identity, and the editor starts a task for the address that replaces it.
//!
//! Native only, for the same reason as [`crate::mux`]: the browser editor
//! has no transport to a runner.

use std::time::{Duration, Instant};

use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};
use tokio::io::AsyncReadExt;
use weida::{IncomingTransfer, OutgoingTransfer, PeerEvent, PeerEvents, Requester, TransferMeta};
use zeughaus_core::GraphDocument;
use zeughaus_link::{GRAPH_MAJOR, GRAPH_PATH, GraphChange, GraphEdit, GraphMessage};

use crate::transport::{Endpoint, QUIC, client_tls, explain, first_dial, policy};

/// Sends edits, each with the request number the runner echoes in a refusal.
pub type EditSender = mpsc::UnboundedSender<(u64, GraphEdit)>;

/// What the graph task reports.
#[derive(Debug, Clone)]
pub enum GraphEvent {
    /// The runner's whole document: what this editor shows for that runner
    /// is exactly this.
    Attached {
        document: GraphDocument,
    },
    Changed(GraphChange),
    /// The runner turned an edit down; the exchange is restarted, which
    /// brings the document back in line with what the runner holds.
    Refused(String),
    /// The exchange ended or the connection dropped. What is on screen is
    /// last-known until the next [`GraphEvent::Attached`].
    Lost,
    /// Weida stopped redialling: this endpoint names a peer that is gone.
    GaveUp,
}

/// How long an exchange has to last to count as a working one. Below it the
/// runner accepted and then ended the exchange, and opening another at once
/// is a full-speed loop on a live connection.
const STABLE_UPTIME: Duration = Duration::from_secs(5);

/// Attaches to a runner's graph and keeps the attachment for as long as the
/// editor wants it.
///
/// `edits` outlives the exchanges: an edit made while disconnected is
/// written after the next attach. The stream ends on `GaveUp`, or when the
/// editor drops the sender of `edits`.
pub fn attach(
    endpoint: Endpoint,
    mut edits: mpsc::UnboundedReceiver<(u64, GraphEdit)>,
) -> impl Stream<Item = GraphEvent> {
    // Room for a burst of changes while the UI is mid-redraw.
    iced::stream::channel(256, async move |mut out| {
        let Some(quic) = QUIC.as_ref() else {
            eprintln!("[graph] no QUIC runtime");
            return;
        };
        let url = match endpoint.path(GRAPH_PATH) {
            Ok(url) => url,
            Err(e) => {
                eprintln!("[graph] {e}");
                return;
            }
        };
        let requester = quic.requester(client_tls());
        // Watched from before the dial, so no transition is missed while the
        // first attach is in flight.
        let mut link = requester.events();
        if let Err(e) = first_dial(&url, || requester.connect(&url)).await {
            eprintln!("[graph] {e}");
            return;
        }

        // An edit taken from the receiver that could not be written: it goes
        // out first after the next attach.
        let mut carry: Option<(u64, GraphEdit)> = None;
        let mut attempt = 0u32;
        loop {
            if attempt > 0 {
                tokio::time::sleep(policy().delay(attempt)).await;
            }
            let started = Instant::now();
            let flow = exchange(&requester, &mut edits, &mut carry, &mut out, &mut link).await;
            attempt = next_attempt(attempt, started.elapsed());
            match flow {
                Flow::Retry => {
                    if out.send(GraphEvent::Lost).await.is_err() {
                        return;
                    }
                }
                Flow::Stop { gave_up } => {
                    if gave_up {
                        let _ = out.send(GraphEvent::GaveUp).await;
                    }
                    return;
                }
            }
        }
    })
}

/// Whether the task opens another exchange.
enum Flow {
    Retry,
    Stop { gave_up: bool },
}

/// The attempt number the next exchange carries, given how long the one that
/// just ended lasted. Never zero, so an exchange that ended is not reopened
/// in the same instant.
fn next_attempt(attempt: u32, lasted: Duration) -> u32 {
    if lasted >= STABLE_UPTIME {
        1
    } else {
        attempt.saturating_add(1).min(16)
    }
}

/// A spawned half of an exchange, stopped when its owner goes away. Iced
/// aborts the outer task by dropping its future, so the guard rather than a
/// tidy exit path is what has to stop the reader.
struct Abort(tokio::task::JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One exchange, from the attach to whatever ends it.
///
/// The reply half is read by a spawned task rather than by a `select!`
/// branch: a frame is a length and then a body, and a `select!` that cancels
/// between the two would leave the stream in the middle of a message.
async fn exchange(
    requester: &Requester,
    edits: &mut mpsc::UnboundedReceiver<(u64, GraphEdit)>,
    carry: &mut Option<(u64, GraphEdit)>,
    out: &mut mpsc::Sender<GraphEvent>,
    link: &mut PeerEvents,
) -> Flow {
    let (mut request, reply) = match requester.open(TransferMeta::default()).await {
        Ok(halves) => halves,
        Err(e) => {
            eprintln!("[graph] open: {e}");
            return Flow::Retry;
        }
    };
    if let Err(e) = write_message(&mut request, &GraphMessage::Attach { major: GRAPH_MAJOR }).await
    {
        eprintln!("[graph] attach: {e}");
        return Flow::Retry;
    }
    let mut body = match reply.recv().await {
        Ok(body) => body,
        Err(e) => {
            eprintln!("[graph] attach: {e}");
            return Flow::Retry;
        }
    };
    let (revision, document) = match read_message(&mut body).await {
        Ok(Some(GraphMessage::Attached { revision, document })) => (revision, document),
        Ok(Some(GraphMessage::Refused { message, .. })) => {
            eprintln!("[graph] attach refused: {message}");
            return Flow::Retry;
        }
        Ok(Some(_)) => {
            eprintln!("[graph] attach answered with an unexpected message");
            return Flow::Retry;
        }
        Ok(None) => return Flow::Retry,
        Err(e) => {
            eprintln!("[graph] attach: {e}");
            return Flow::Retry;
        }
    };
    if out.send(GraphEvent::Attached { document }).await.is_err() {
        return Flow::Stop { gave_up: false };
    }

    let mut reader = Abort(tokio::spawn(read_changes(body, out.clone(), revision)));
    if let Some((id, edit)) = carry.take()
        && let Err(e) = write_edit(&mut request, id, edit, carry).await
    {
        eprintln!("[graph] edit: {e}");
        return Flow::Retry;
    }
    let flow = loop {
        tokio::select! {
            edit = edits.next() => {
                // The app dropped its sender: this endpoint is no longer
                // wanted, and the task with it.
                let Some((id, edit)) = edit else {
                    break Flow::Stop { gave_up: false };
                };
                if let Err(e) = write_edit(&mut request, id, edit, carry).await {
                    eprintln!("[graph] edit: {e}");
                    break Flow::Retry;
                }
            }
            // The runner ended the reply half, or sent something that ends
            // the exchange: what is on screen is last-known until the next
            // attach.
            _ = &mut reader.0 => break Flow::Retry,
            event = link.recv() => match event {
                None => break Flow::Stop { gave_up: false },
                Some(PeerEvent::Lost { cause, .. }) => {
                    eprintln!("[graph] runner lost: {cause}");
                    break Flow::Retry;
                }
                Some(PeerEvent::GaveUp { why, .. }) => {
                    eprintln!("[graph] {}", explain(&why));
                    break Flow::Stop { gave_up: true };
                }
                Some(_) => {}
            },
        }
    };
    reader.0.abort();
    flow
}

/// Writes one edit, keeping it in `carry` when the write fails so the next
/// exchange sends it.
async fn write_edit(
    request: &mut OutgoingTransfer,
    id: u64,
    edit: GraphEdit,
    carry: &mut Option<(u64, GraphEdit)>,
) -> Result<(), String> {
    let message = GraphMessage::Edit { request: id, edit };
    let written = write_message(request, &message).await;
    if written.is_err()
        && let GraphMessage::Edit { edit, .. } = message
    {
        *carry = Some((id, edit));
    }
    written
}

/// Reads the exchange's reply half until it ends, forwarding what the app
/// acts on. Ends the exchange (by returning) on a gap in the revisions, a
/// refusal -- after forwarding it -- or anything it does not understand.
async fn read_changes(mut body: IncomingTransfer, mut out: mpsc::Sender<GraphEvent>, start: u64) {
    let mut last = start;
    loop {
        let message = match read_message(&mut body).await {
            Ok(Some(message)) => message,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[graph] {e}");
                return;
            }
        };
        match message {
            GraphMessage::Changed { revision, change } => {
                if revision != last.wrapping_add(1) {
                    eprintln!("[graph] revision {revision} after {last}: attaching again");
                    return;
                }
                last = revision;
                if out.send(GraphEvent::Changed(change)).await.is_err() {
                    return;
                }
            }
            // A reader that fell too far behind is sent the whole document
            // again instead of the changes it missed.
            GraphMessage::Attached { revision, document } => {
                last = revision;
                if out.send(GraphEvent::Attached { document }).await.is_err() {
                    return;
                }
            }
            GraphMessage::Refused { message, .. } => {
                let _ = out.send(GraphEvent::Refused(message)).await;
                return;
            }
            _ => {
                eprintln!("[graph] unexpected message from the runner");
                return;
            }
        }
    }
}

/// Writes one message. Encoded whole first, so a frame is either written
/// completely or not at all.
async fn write_message(out: &mut OutgoingTransfer, message: &GraphMessage) -> Result<(), String> {
    let frame = message.encode()?;
    out.write_all(&frame)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// Reads one message, or `None` when the stream ended cleanly. The length is
/// validated before the body is allocated.
async fn read_message(body: &mut IncomingTransfer) -> Result<Option<GraphMessage>, String> {
    let mut header = [0u8; 4];
    // A stream that ended arrives here as a short read, not as an error.
    if body.read_exact(&mut header).await.is_err() {
        return Ok(None);
    }
    let length = GraphMessage::body_len(header)?;
    let mut payload = vec![0u8; length];
    body.read_exact(&mut payload)
        .await
        .map_err(|e| format!("body: {e}"))?;
    GraphMessage::decode(&payload).map(Some)
}
