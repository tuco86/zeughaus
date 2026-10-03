//! The editor's client for the runtime's sample feed: how a video signal gets
//! from the process that computes it to the node body that draws it.
//!
//! Frames are the one thing the shared store does not carry. A 3840x2160 RGBA
//! frame is 33 MB, per frame, replicated to every subscriber; scalars go through
//! SpacetimeDB, pixels get their own QUIC connection. This module is the
//! dialling half of that connection -- one long-lived exchange per (source node,
//! pin), asking for the size the node body actually draws, reading
//! [`FrameHeader`]-prefixed frames until the editor stops wanting them -- and
//! of the runtime's event stream beside it.
//!
//! Nothing here redials. Every address is dialled once through
//! [`first_dial`] and kept alive by weida from then on; what this module owns
//! is what a redial does not restore: the snapshot a subscription needs after
//! a gap, and the exchange a feed needs after its stream ended.
//!
//! Native only: the wasm editor has no sync layer, so it never learns where a
//! runtime serves frames and has nothing to dial.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream};
use tokio::io::AsyncReadExt;
use weida::{PeerEvent, PeerEvents, Requester, TransferMeta};
use zeughaus_core::Image;
use zeughaus_link::{
    BUSY_PATH, BusyMode, BusyRequest, EVENTS_PATH, FEED_PATH, FeedRequest, FrameHeader, HOLD_PATH,
    HoldReply, HoldRequest, MAX_BUSY_BYTES, MAX_EVENT_BYTES, MAX_HOLD_BYTES, MAX_SNAPSHOT_BYTES,
    MachineState, RuntimeEvent, SNAPSHOT_PATH, Snapshot, TRIGGERS_PATH, TriggerRequest, ladder,
};

use crate::transport::{Endpoint, QUIC, client_tls, explain, first_dial, gave_up, policy};

/// Which feed: one output pin of one node in the shared graph.
///
/// The *source* pin, not the Display node that draws it -- two Display nodes
/// watching the same pin are one feed, because the frame they need is the same
/// frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FeedKey {
    pub node_id: u64,
    pub pin: Arc<str>,
}

/// Everything one feed task needs: where to dial, what to ask for, and which
/// generation of the feed it is.
#[derive(Debug, Clone)]
pub struct FeedSpec {
    pub endpoint: Endpoint,
    pub key: FeedKey,
    /// Distinguishes this feed from the one it replaced. See [`Frame::epoch`].
    pub epoch: u64,
    /// Requested frame height, snapped to a [`ladder`] tier. `0` means "source
    /// resolution". See [`requested_tier`] for why the width is not part of it.
    pub tier: u32,
}

/// Orders frames within one feed.
///
/// `seq` is monotonic within one QUIC stream, but a feed redials after a drop
/// and the new stream numbers its frames from zero -- so the stream counter has
/// to come first, or every frame after a reconnect would look older than the
/// last one before it. Lexicographic by derive: stream, then sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameOrder {
    pub stream: u64,
    pub seq: u64,
}

/// One frame on its way to the UI.
pub struct Frame {
    pub key: FeedKey,
    /// Which feed generation produced it. Assigned by the editor when it opened
    /// the feed, so a frame still in flight from a feed that has been replaced
    /// -- by a resize, an endpoint change, a graph reload -- is recognizable and
    /// can be refused.
    pub epoch: u64,
    pub order: FrameOrder,
    pub image: Image,
}

/// Cloning shares the pixels; `Image` is `Arc`-backed, so this is not a copy of
/// the frame. `Message` requires `Clone` and a frame must not pay 33 MB for it.
impl Clone for Frame {
    fn clone(&self) -> Self {
        Frame {
            key: self.key.clone(),
            epoch: self.epoch,
            order: self.order,
            image: self.image.clone(),
        }
    }
}

/// Geometry only: `Message` derives `Debug`, and a derived one here would print
/// a frame's every byte into a log line.
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("node_id", &self.key.node_id)
            .field("pin", &self.key.pin)
            .field("epoch", &self.epoch)
            .field("stream", &self.order.stream)
            .field("seq", &self.order.seq)
            .field("width", &self.image.width())
            .field("height", &self.image.height())
            .finish()
    }
}

/// The frame height to ask for, given the size a node body draws at.
///
/// The height only. The runtime bounds the height by a [`ladder`] tier and takes
/// the aspect ratio from the source, so a 3840x2160 frame served at tier 240
/// arrives as 427x240 -- wider than a 240-wide node body, which the node's
/// `ContentFit::Contain` letterboxes. Asking for a width as well would not
/// narrow that frame; it would only split one tier into a separate scaling pass
/// per viewer width, which is exactly the sharing the ladder exists for.
///
/// Snapping to a tier is what keeps a resize from restarting the feed: only
/// crossing a tier changes the request, not every pixel of a drag. A body taller
/// than the largest tier asks for `0`, the protocol's "send it at source
/// resolution" -- at that size there is nothing left to save by scaling. A
/// degenerate zero-height body is clamped to one pixel instead, so a node
/// momentarily laid out at zero does not accidentally ask for a 33 MB frame.
pub fn requested_tier(size: iced::Size) -> u32 {
    ladder::tier_for(size.height.max(1.0) as u32).unwrap_or(0)
}

/// The tier that satisfies both requests, for two Display nodes sharing a feed.
///
/// `0` means "source resolution", so it wins over any finite tier rather than
/// losing a plain `max`.
pub fn widen(a: u32, b: u32) -> u32 {
    if a == 0 || b == 0 { 0 } else { a.max(b) }
}

/// What the runtime told this editor about the graph it is executing.
#[derive(Debug, Clone)]
pub enum Traffic {
    /// The whole current output set, answering a late join.
    Snapshot(Snapshot),
    /// One thing that just happened.
    Event(RuntimeEvent),
    /// The subscription dropped; values are stale until the next `Snapshot`.
    Lost,
}

/// Subscribes to a runtime's events and keeps them coming for as long as the
/// editor wants them.
///
/// The stream never ends on its own, for the same reason [`frames`] does not:
/// the editor stops wanting this by dropping the task. Weida keeps the two
/// addresses -- the event topic and the snapshot endpoint -- alive across a
/// lost connection and re-sends the filter on the redialled one. What it does
/// not restore is what was published in the gap, so every `Connected` asks for
/// a snapshot, and every `Lost` says so downstream.
///
/// The stream does end when weida gives the address up: a runner that
/// restarted has a fresh identity, so the address that was dialled names a
/// peer that no longer exists. The store announces the replacement, and this
/// task is replaced by one that dials it.
pub fn events(endpoint: Endpoint) -> impl Stream<Item = Traffic> {
    // Room for a burst of events while the UI is mid-redraw. Unlike frames
    // these are a few hundred bytes each, so buffering them is cheap and
    // dropping one loses a value nothing else will restate.
    iced::stream::channel(64, async move |mut out| {
        let Some(quic) = QUIC.as_ref() else {
            eprintln!("[traffic] no QUIC runtime");
            return;
        };
        let (events_url, snapshot_url) =
            match (endpoint.path(EVENTS_PATH), endpoint.path(SNAPSHOT_PATH)) {
                (Ok(events), Ok(snapshot)) => (events, snapshot),
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("[traffic] {e}");
                    return;
                }
            };

        let subscriber = quic.subscriber(client_tls());
        // Watched from before the dial, so the first `Connected` is seen like
        // every later one: it is what asks for a snapshot.
        let mut link = subscriber.events();
        // The empty filter is every topic: outputs and edge traffic alike.
        // Registered before the dial, so it is sent as part of attaching to
        // every connection the address gets, the first and each redial alike;
        // subscribing before asking for the snapshot is what keeps an event
        // between the two from being lost, with `seq` resolving the overlap.
        if let Err(e) = subscriber.subscribe("").await {
            eprintln!("[traffic] subscribe: {e}");
            return;
        }
        let requester = quic.requester(client_tls());
        let mut snapshots = requester.events();
        if let Err(e) = first_dial(&events_url, || subscriber.connect(&events_url)).await {
            eprintln!("[traffic] {e}");
            return;
        }
        if let Err(e) = first_dial(&snapshot_url, || requester.connect(&snapshot_url)).await {
            eprintln!("[traffic] {e}");
            return;
        }

        // Whether a snapshot is on screen. Only then is there state that can
        // go stale, and only then is a loss worth reporting.
        let mut served = false;
        loop {
            tokio::select! {
                event = link.recv() => {
                    let resync = match event {
                        // The endpoint is gone, which for one this task owns
                        // means the task is ending.
                        None => return,
                        Some(PeerEvent::Connected { .. }) => true,
                        Some(PeerEvent::Retrying { .. }) => continue,
                        Some(PeerEvent::Lost { cause, .. }) => {
                            eprintln!("[traffic] runtime lost: {cause}");
                            false
                        }
                        // Whether a `Connected` was among the missed ones is
                        // unknowable; what the address is now is not.
                        Some(PeerEvent::Missed(n)) => {
                            eprintln!("[traffic] missed {n} peer event(s)");
                            subscriber.peer_count() > 0
                        }
                        Some(PeerEvent::GaveUp { why, .. }) => {
                            eprintln!("[traffic] {}", explain(&why));
                            if served {
                                let _ = out.send(Traffic::Lost).await;
                            }
                            return;
                        }
                    };
                    if resync {
                        match snapshot(&requester, &mut snapshots).await {
                            Ok(snapshot) => {
                                if out.send(Traffic::Snapshot(snapshot)).await.is_err() {
                                    return;
                                }
                                served = true;
                            }
                            Err(e) => eprintln!("[traffic] {e}"),
                        }
                    } else if served {
                        served = false;
                        // Values on screen are last-known, not wrong; saying
                        // so is the honest state until a snapshot replaces them.
                        if out.send(Traffic::Lost).await.is_err() {
                            return;
                        }
                    }
                }
                message = subscriber.recv() => {
                    let message = match message {
                        Ok(message) => message,
                        Err(e) => {
                            eprintln!("[traffic] subscription: {e}");
                            return;
                        }
                    };
                    let payload = match message.collect(MAX_EVENT_BYTES).await {
                        Ok(payload) => payload,
                        Err(e) => {
                            eprintln!("[traffic] event: {e}");
                            continue;
                        }
                    };
                    let Some(event) = RuntimeEvent::decode(&payload) else {
                        eprintln!(
                            "[traffic] skipped a malformed event ({} bytes)",
                            payload.len()
                        );
                        continue;
                    };
                    if out.send(Traffic::Event(event)).await.is_err() {
                        return;
                    }
                }
            }
        }
    })
}

/// Asks the runtime for its whole current output set.
///
/// The snapshot endpoint is its own address and comes back on its own
/// schedule, so a request that fails on the connection waits for that
/// address's next `Connected` rather than for the subscription's. A request
/// on an address weida is still redialling waits inside `request`; one on an
/// address weida gave up fails at once, which is why the wait ends on
/// `GaveUp` instead of turning into a loop on the loss cause.
async fn snapshot(requester: &Requester, link: &mut PeerEvents) -> Result<Snapshot, String> {
    loop {
        let reply = match requester.request(b"").await {
            Ok(reply) => reply,
            Err(e @ (weida::Error::ConnectionLost(_) | weida::Error::Indeterminate)) => {
                eprintln!("[traffic] snapshot: {e}; waiting for the endpoint");
                loop {
                    match link.recv().await {
                        Some(PeerEvent::Connected { .. } | PeerEvent::Missed(_)) => break,
                        Some(PeerEvent::GaveUp { why, .. }) => {
                            return Err(format!("snapshot: {}", explain(&why)));
                        }
                        None => return Err("snapshot: endpoint gone".to_owned()),
                        Some(_) => {}
                    }
                }
                continue;
            }
            Err(e) => return Err(format!("snapshot: {e}")),
        };
        let encoded = reply
            .collect(MAX_SNAPSHOT_BYTES)
            .await
            .map_err(|e| format!("snapshot: {e}"))?;
        return Snapshot::decode(&encoded).ok_or_else(|| "snapshot: malformed".to_owned());
    }
}

/// How long a feed has to last to count as a working one.
///
/// Below this it is a runtime that accepts and then ends the exchange -- a pin
/// it no longer serves, a standby that does not execute, a burst it refused --
/// and asking it again immediately is a full-speed loop of exchanges on a
/// live connection. Above it, the runtime was serving and merely stopped,
/// which deserves the shortest wait there is.
const STABLE_UPTIME: Duration = Duration::from_secs(5);

/// The attempt number the next exchange carries, given how long the one that
/// just ended lasted.
///
/// Never zero: the loop sleeps for any attempt above zero, and an exchange
/// that ended -- however well it had been going -- must not be reopened in
/// the same instant. Resetting to the *first* delay rather than to none is
/// what bounds it: a runtime that served and then ended the stream would
/// otherwise reset the backoff to nothing and be asked again at once, forever.
fn next_attempt(attempt: u32, lasted: Duration) -> u32 {
    if lasted >= STABLE_UPTIME {
        return 1;
    }
    attempt.saturating_add(1)
}

/// Asks the runtime to fire a node once.
///
/// Push, not request: a press has no answer worth waiting for -- the editor
/// learns it worked by seeing the value change. No payload: a press is the
/// bare trigger, and what a node makes of text it was fired with is the
/// business of whoever sends text.
pub async fn trigger(endpoint: Endpoint, node_id: u64) -> Result<(), String> {
    let quic = QUIC.as_ref().ok_or("no QUIC runtime")?;
    let url = endpoint.path(TRIGGERS_PATH)?;
    let pusher = quic.pusher(client_tls());
    pusher
        .connect(&url)
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;
    pusher
        .send(
            &TriggerRequest {
                node_id,
                payload: None,
                external: false,
            }
            .encode(),
        )
        .await
        .map_err(|e| format!("trigger {node_id}: {e}"))
}

/// Holds the runner or releases it, and reports what it answered.
///
/// Request and reply, unlike [`trigger`]: the point of holding is to watch
/// the live runs drain, and the count that says whether they have comes back
/// with the acknowledgement.
pub async fn hold(endpoint: Endpoint, held: bool) -> Result<HoldReply, String> {
    let quic = QUIC.as_ref().ok_or("no QUIC runtime")?;
    let url = endpoint.path(HOLD_PATH)?;
    let requester = quic.requester(client_tls());
    first_dial(&url, || requester.connect(&url)).await?;
    let reply = requester
        .request(&HoldRequest { held }.encode())
        .await
        .map_err(|e| format!("hold: {e}"))?;
    let encoded = reply
        .collect(MAX_HOLD_BYTES)
        .await
        .map_err(|e| format!("hold: {e}"))?;
    HoldReply::decode(&encoded).ok_or_else(|| "hold: malformed".to_owned())
}

/// Sets a CI runner's busy mode, and reports the machine's state after it.
pub async fn set_busy(endpoint: Endpoint, mode: BusyMode) -> Result<MachineState, String> {
    let quic = QUIC.as_ref().ok_or("no QUIC runtime")?;
    let url = endpoint.path(BUSY_PATH)?;
    let requester = quic.requester(client_tls());
    first_dial(&url, || requester.connect(&url)).await?;
    let reply = requester
        .request(&BusyRequest { mode }.encode())
        .await
        .map_err(|e| format!("busy: {e}"))?;
    let encoded = reply
        .collect(MAX_BUSY_BYTES)
        .await
        .map_err(|e| format!("busy: {e}"))?;
    MachineState::decode(&encoded).ok_or_else(|| "busy: malformed".to_owned())
}

/// Streams one feed's frames for as long as the editor wants them.
///
/// The stream never ends on its own. A runtime that ends the exchange -- the
/// source stopped producing, the node went away, the network blinked -- is a
/// reason to ask again, not a reason to stop wanting video: the editor decides
/// that, and it decides it by dropping this task. Each attempt is a fresh
/// stream numbering its frames from zero, hence [`FrameOrder::stream`].
///
/// A lost connection is weida's to redial: `open` waits for it. The wait
/// between attempts here is for the exchange the runtime ended on a live
/// connection, which no redial paces. The stream does end when weida gives
/// the address up -- a restarted runner is a stranger to it -- because every
/// `open` from then on would fail at once, and the store's new address
/// replaces this task anyway.
pub fn frames(spec: FeedSpec) -> impl Stream<Item = Frame> {
    // Capacity zero: the futures channel still admits one message per sender, so
    // at most one frame waits for the UI while the next is being read. Any more
    // and a viewer that redraws slowly would accumulate frames at 33 MB each
    // instead of just receiving fewer of them.
    iced::stream::channel(0, async move |mut out| {
        let label = format!("[feed] node {} pin {}", spec.key.node_id, spec.key.pin);
        let Some(quic) = QUIC.as_ref() else {
            eprintln!("{label}: no QUIC runtime");
            return;
        };
        let url = match spec.endpoint.path(FEED_PATH) {
            Ok(url) => url,
            Err(e) => {
                eprintln!("{label}: {e}");
                return;
            }
        };
        let requester = quic.requester(client_tls());
        let mut link = requester.events();
        if let Err(e) = first_dial(&url, || requester.connect(&url)).await {
            eprintln!("{label}: {e}");
            return;
        }

        let policy = policy();
        let mut attempt = 0u32;
        for stream in 0.. {
            if attempt > 0 {
                tokio::time::sleep(policy.delay(attempt)).await;
            }
            let started = Instant::now();
            tokio::select! {
                result = pump(&requester, &spec, stream, &mut out) => match result {
                    // The receiver is gone: nobody is drawing this feed any more.
                    Ok(Wanted::No) => return,
                    Ok(Wanted::Yes) => {}
                    Err(e) => eprintln!("{label}: {e}"),
                },
                why = gave_up(&mut link) => {
                    eprintln!("{label}: {}", explain(&why));
                    return;
                }
            }
            attempt = next_attempt(attempt, started.elapsed());
        }
    })
}

/// Whether the editor is still drawing a feed.
enum Wanted {
    Yes,
    No,
}

/// Opens the exchange, sends the request once, then reads frames until the
/// stream ends or the receiver goes away.
async fn pump(
    requester: &Requester,
    spec: &FeedSpec,
    stream: u64,
    out: &mut mpsc::Sender<Frame>,
) -> Result<Wanted, String> {
    let (mut request, reply) = requester
        .open(TransferMeta::default())
        .await
        .map_err(|e| format!("open: {e}"))?;
    // Width `0`: no bound. The tier bounds the height and the source's aspect
    // ratio does the rest -- see `requested_tier`.
    let ask = FeedRequest::new(spec.key.node_id, &*spec.key.pin, 0, spec.tier);
    request
        .write_all(&ask.encode())
        .await
        .map_err(|e| format!("request: {e}"))?;
    // Finishing the request half is what tells the runtime the terms are
    // complete and it may start sending; a standing request is written once, not
    // once per frame.
    request.finish().map_err(|e| format!("request: {e}"))?;

    let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
    let mut header = [0u8; FrameHeader::LEN];
    loop {
        // The feed ending is a clean end of stream, which arrives here as a
        // short read on the header rather than as an error.
        if body.read_exact(&mut header).await.is_err() {
            return Ok(Wanted::Yes);
        }
        let Some(head) = FrameHeader::decode(&header) else {
            return Err("implausible frame geometry".to_owned());
        };
        // One frame's pixels at a time. This buffer is the only place a frame is
        // materialized on this side, and it is handed straight to `Image`, which
        // shares it from there on.
        let mut pixels = vec![0u8; head.payload_len()];
        body.read_exact(&mut pixels)
            .await
            .map_err(|e| format!("frame {}: {e}", head.seq))?;
        let frame = Frame {
            key: spec.key.clone(),
            epoch: spec.epoch,
            order: FrameOrder {
                stream,
                seq: head.seq,
            },
            image: Image::from_rgba(head.width, head.height, pixels),
        };
        // Sending is also the backpressure: while the UI has not taken the last
        // frame, nothing is read from the stream, and QUIC flow control slows
        // the runtime down instead of queueing frames here.
        if out.send(frame).await.is_err() {
            return Ok(Wanted::No);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of snapping: a drag that stays inside a tier must produce the
    /// same request, because a different request restarts the feed. Widening the
    /// body must not either -- the runtime bounds the height, not the width.
    #[test]
    fn a_drag_within_a_tier_asks_for_the_same_tier() {
        let a = requested_tier(iced::Size::new(240.0, 150.0));
        assert_eq!(a, requested_tier(iced::Size::new(238.0, 161.0)));
        assert_eq!(a, requested_tier(iced::Size::new(900.0, 150.0)));
        assert_eq!(a, 240);
    }

    /// Crossing a tier is the one thing that must change the request.
    #[test]
    fn crossing_a_tier_asks_for_a_bigger_one() {
        assert_eq!(requested_tier(iced::Size::new(240.0, 300.0)), 360);
    }

    /// Past the largest tier there is nothing left to save by scaling, so the
    /// request becomes the protocol's "source resolution".
    #[test]
    fn a_body_past_the_ladder_asks_for_source_resolution() {
        assert_eq!(requested_tier(iced::Size::new(2000.0, 1500.0)), 0);
    }

    /// A zero-sized layout must not turn into a full-resolution request.
    #[test]
    fn a_degenerate_size_asks_for_the_smallest_tier() {
        assert_eq!(requested_tier(iced::Size::new(0.0, 0.0)), 240);
    }

    /// Two viewers on one pin share a feed, so the shared request has to satisfy
    /// the larger of them -- and "source resolution" is larger than any tier.
    #[test]
    fn a_shared_request_satisfies_both_viewers() {
        assert_eq!(widen(240, 360), 360);
        assert_eq!(widen(240, 0), 0);
    }

    /// Ordering has to put the stream first, or the first frame after a
    /// reconnect (sequence back to zero) would look older than the last one
    /// before it and be dropped forever.
    #[test]
    fn a_reconnected_stream_outranks_the_one_before_it() {
        let before = FrameOrder {
            stream: 0,
            seq: 900,
        };
        let after = FrameOrder { stream: 1, seq: 0 };
        assert!(after > before);
    }

    /// The loop this schedule drives sleeps for any attempt above zero, so
    /// what has to hold is: never zero, growing while the runtime keeps
    /// ending the exchange, and back to the shortest wait once one exchange
    /// actually lasted. Resetting to no wait at all asks the runtime again at
    /// once and pays an exchange per turn on a live connection.
    #[test]
    fn an_ended_exchange_always_costs_at_least_one_backoff_step() {
        let brief = Duration::from_millis(20);
        assert_eq!(next_attempt(0, brief), 1, "even the first end waits");
        assert_eq!(next_attempt(1, brief), 2);
        assert_eq!(next_attempt(2, brief), 3, "one that keeps ending waits");
        // Just short of stable is still not stable.
        assert_eq!(next_attempt(3, STABLE_UPTIME - brief), 4);

        // An exchange that lasted is a runtime that stopped, not one that
        // refuses: the next ask is the shortest wait, not none.
        assert_eq!(next_attempt(7, STABLE_UPTIME), 1);
        assert_eq!(next_attempt(7, STABLE_UPTIME * 100), 1);

        // And no attempt count wraps back to "ask again immediately".
        assert_eq!(next_attempt(u32::MAX, brief), u32::MAX);
    }

    /// One incarnation of a runtime's transport side: a bound listener with
    /// the event topic, the snapshot service and a feed that answers every
    /// request with one 2x2 frame stamped `seq` and then ends the exchange.
    struct FakeRuntime {
        runtime: weida::Runtime,
        binding: weida::Binding,
        publisher: weida::Publisher,
    }

    impl FakeRuntime {
        async fn bind(port: u16, identity: weida::Identity, seq: u64) -> FakeRuntime {
            let runtime = weida::Runtime::new(weida::RuntimeConfig::default()).expect("runtime");
            let listener = runtime.listener();
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let binding = listener.bind_quic(addr, identity).await.expect("bind");
            let publisher = listener.publisher(EVENTS_PATH).expect("publisher");
            let replier = listener.replier(SNAPSHOT_PATH).expect("replier");
            tokio::spawn(async move {
                while let Ok(mut request) = replier.accept().await {
                    drop(request.take_body());
                    let snapshot = Snapshot {
                        seq,
                        ..Snapshot::default()
                    };
                    let mut reply = request.reply(TransferMeta::default()).await.expect("reply");
                    reply.write_all(&snapshot.encode()).await.expect("write");
                    reply.finish().expect("finish");
                }
            });
            let feed = listener.replier(FEED_PATH).expect("feed replier");
            tokio::spawn(async move {
                while let Ok(mut request) = feed.accept().await {
                    let ask = request
                        .take_body()
                        .read_capped(4096)
                        .await
                        .expect("request");
                    assert!(FeedRequest::decode(&ask).is_some(), "a well-formed request");
                    let mut reply = request.reply(TransferMeta::default()).await.expect("reply");
                    let header = FrameHeader::new(seq, 2, 2);
                    reply.write_all(&header.encode()).await.expect("header");
                    reply.write_all(&[7u8; 16]).await.expect("pixels");
                    reply.finish().expect("finish");
                }
            });
            FakeRuntime {
                runtime,
                binding,
                publisher,
            }
        }

        fn port(&self) -> u16 {
            self.binding.local_addr().port()
        }

        /// Publishes until the subscriber is there to receive: a subscription
        /// registered on a redialled connection lands a moment after the
        /// address reports `Connected`.
        async fn publish(&self, seq: u64) {
            let event = RuntimeEvent::Edge { seq, edge_id: 1 };
            for _ in 0..100 {
                if self.publisher.subscriber_count() > 0 {
                    self.publisher
                        .publish(zeughaus_link::TOPIC_EDGE, event.encode())
                        .expect("publish");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no subscriber arrived");
        }

        /// Ends this incarnation the way a stopping runner does: every
        /// connection closed with a shutdown, then the socket released.
        async fn stop(self) {
            self.binding.close().await;
            self.runtime.shutdown().await;
        }
    }

    /// The whole point of the cutover: a runtime that goes away and comes
    /// back under the same identity on the same port is redialled by weida,
    /// and this side's only work is to say `Lost`, fetch the snapshot the
    /// gap needs, and reopen the feed exchange. No second dial happens here
    /// -- there is no loop left that could make one. Loopback QUIC, so it
    /// needs a reactor; the process-wide client runtime binds to this test's
    /// reactor, which is why it is the one network test in this module.
    #[tokio::test]
    async fn a_restarted_runtime_is_redialled_by_weida_not_by_us() {
        use iced::futures::StreamExt;
        use weida::EndpointAddr;

        let identity = weida::Identity::generate_for(["127.0.0.1"]).expect("identity");
        let fingerprint = identity.fingerprint().expect("fingerprint");
        let first = FakeRuntime::bind(0, identity.clone(), 1).await;
        let port = first.port();
        let endpoint = Endpoint(
            EndpointAddr {
                host: "127.0.0.1".to_owned(),
                port: Some(port),
                path: "/".to_owned(),
                peer: Some(fingerprint),
            }
            .to_string(),
        );

        let mut traffic = std::pin::pin!(events(endpoint.clone()));
        let mut video = std::pin::pin!(frames(FeedSpec {
            endpoint,
            key: FeedKey {
                node_id: 9,
                pin: Arc::from("frame"),
            },
            epoch: 0,
            tier: 240,
        }));
        async fn next<T>(stream: &mut (impl Stream<Item = T> + Unpin)) -> T {
            tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .expect("an item within 10 s")
                .expect("stream still open")
        }

        assert!(matches!(next(&mut traffic).await, Traffic::Snapshot(s) if s.seq == 1));
        first.publish(2).await;
        assert!(matches!(
            next(&mut traffic).await,
            Traffic::Event(RuntimeEvent::Edge { seq: 2, .. })
        ));
        let frame = next(&mut video).await;
        assert_eq!((frame.order.stream, frame.order.seq), (0, 1));
        assert_eq!(frame.image.width(), 2);

        first.stop().await;
        assert!(matches!(next(&mut traffic).await, Traffic::Lost));

        let second = FakeRuntime::bind(port, identity, 3).await;
        assert!(matches!(next(&mut traffic).await, Traffic::Snapshot(s) if s.seq == 3));
        second.publish(4).await;
        assert!(matches!(
            next(&mut traffic).await,
            Traffic::Event(RuntimeEvent::Edge { seq: 4, .. })
        ));
        // The feed's exchange was reopened on the redialled connection: a
        // later stream number, and the second incarnation's frame on it.
        let frame = next(&mut video).await;
        assert!(frame.order.stream > 0, "a fresh stream after the restart");
        assert_eq!(frame.order.seq, 3);
        second.stop().await;
        assert!(matches!(next(&mut traffic).await, Traffic::Lost));

        // A runner restarted with a fresh identity is a stranger to the
        // address: weida refuses it in the handshake and gives the address
        // up, and both streams end rather than looping on the loss. The
        // store's new address is what replaces them.
        // Bounded by the redial schedule: two addresses back off
        // independently up to 4 s per attempt, and the feed's own re-open
        // pace sits on top, so the sum can pass ten seconds on a busy host.
        let stranger = weida::Identity::generate_for(["127.0.0.1"]).expect("identity");
        let third = FakeRuntime::bind(port, stranger, 5).await;
        let ended = tokio::time::timeout(Duration::from_secs(30), traffic.next())
            .await
            .expect("the event stream ends within 30 s");
        assert!(ended.is_none(), "got {ended:?} from a stranger");
        let ended = tokio::time::timeout(Duration::from_secs(30), video.next())
            .await
            .expect("the feed ends within 30 s");
        assert!(ended.is_none(), "got {ended:?} from a stranger");
        third.stop().await;
    }
}
