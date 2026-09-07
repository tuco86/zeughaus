//! The editor's client for the runtime's sample feed: how a video signal gets
//! from the process that computes it to the node body that draws it.
//!
//! Frames are the one thing the shared store does not carry. A 3840x2160 RGBA
//! frame is 33 MB, per frame, replicated to every subscriber; scalars go through
//! SpacetimeDB, pixels get their own QUIC connection. This module is the
//! dialling half of that connection -- one long-lived exchange per (source node,
//! pin), asking for the size the node body actually draws, reading
//! [`FrameHeader`]-prefixed frames until the editor stops wanting them.
//!
//! Native only: the wasm editor has no sync layer, so it never learns where a
//! runtime serves frames and has nothing to dial.

use std::fmt;
use std::sync::{Arc, LazyLock};

use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream};
use tokio::io::AsyncReadExt;
use weida::{ClientTls, EndpointAddr, Runtime, RuntimeConfig, TransferMeta, Trust};
use zeughaus_core::Image;
use zeughaus_samples::{FEED_PATH, FeedRequest, FrameHeader, ladder};

/// The one QUIC client this process needs.
///
/// A process-wide `Runtime` rather than one per feed, because connections are
/// pooled per (address, trust anchors) inside it: several Display nodes watching
/// the same runner then share one QUIC connection instead of each opening its
/// own UDP socket and handshake.
///
/// Built on first use, which happens inside a feed task and therefore inside
/// iced's tokio runtime -- `Runtime::new` requires an ambient reactor, and the
/// editor's `App::new` runs before there is one.
static QUIC: LazyLock<Option<Runtime>> = LazyLock::new(|| {
    match Runtime::new(RuntimeConfig::default()) {
        Ok(runtime) => Some(runtime),
        // Not fatal: the editor still edits graphs, it just cannot show video.
        Err(e) => {
            eprintln!("[feed] no QUIC runtime, video disabled: {e}");
            None
        }
    }
});

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

/// Where a runtime is reachable: the pinned root URL it announced.
///
/// Compared for equality to notice a runner that restarted on a different port
/// or with a fresh identity; every feed dialled the old one and has to be
/// redialled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(pub String);

impl Endpoint {
    /// The URL of one endpoint path on this runtime.
    ///
    /// The announced URL is the root (`/`); every path this editor dials is the
    /// same authority and the same pinned fingerprint with the path replaced,
    /// so a runtime announces one address and not a list.
    pub fn path(&self, path: &str) -> Result<String, String> {
        let mut addr =
            EndpointAddr::parse(&self.0).map_err(|e| format!("endpoint {}: {e}", self.0))?;
        addr.path = path.to_owned();
        Ok(addr.to_string())
    }
}

/// The trust every dial uses: the fingerprint pinned in the URL the runtime
/// announced, and nothing else. This is the client-side swap point for a later
/// mTLS integration.
fn client_tls() -> ClientTls {
    ClientTls::new(Trust::by_address())
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

/// Streams one feed's frames for as long as the editor wants them.
///
/// The stream never ends on its own. A runtime that drops the exchange -- it
/// restarted, the source stopped producing, the network blinked -- is a reason
/// to redial, not a reason to stop wanting video: the editor decides that, and
/// it decides it by dropping this task. Each attempt is a fresh stream numbering
/// its frames from zero, hence [`FrameOrder::stream`].
pub fn frames(spec: FeedSpec) -> impl Stream<Item = Frame> {
    // Capacity zero: the futures channel still admits one message per sender, so
    // at most one frame waits for the UI while the next is being read. Any more
    // and a viewer that redraws slowly would accumulate frames at 33 MB each
    // instead of just receiving fewer of them.
    iced::stream::channel(0, async move |mut out| {
        for stream in 0.. {
            if stream > 0 {
                // Backing off matters because the common reason to be here is a
                // runtime that is not serving: redialling at frame rate would
                // spend a handshake per attempt on a peer that has nothing.
                tokio::time::sleep(retry_delay(stream)).await;
            }
            match pump(&spec, stream, &mut out).await {
                // The receiver is gone: nobody is drawing this feed any more.
                Ok(Wanted::No) => return,
                Ok(Wanted::Yes) => {}
                Err(e) => eprintln!("[feed] node {} pin {}: {e}", spec.key.node_id, spec.key.pin),
            }
        }
    })
}

/// Whether the editor is still drawing a feed.
enum Wanted {
    Yes,
    No,
}

/// How long to wait before redialling after `attempt` failures: doubling from
/// 250 ms, capped at 4 s. Long enough that a runtime restart is not a storm,
/// short enough that a viewer notices the runtime coming back.
fn retry_delay(attempt: u64) -> std::time::Duration {
    let ms = 250u64 << attempt.min(4);
    std::time::Duration::from_millis(ms)
}

/// Dials the feed, sends the request once, then reads frames until the stream
/// ends or the receiver goes away.
async fn pump(
    spec: &FeedSpec,
    stream: u64,
    out: &mut mpsc::Sender<Frame>,
) -> Result<Wanted, String> {
    let quic = QUIC.as_ref().ok_or("no QUIC runtime")?;
    let url = spec.endpoint.path(FEED_PATH)?;
    let requester = quic.requester(client_tls());
    requester
        .connect(&url)
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;

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

    /// Backoff is bounded in both directions: quick enough to notice a runtime
    /// coming back, slow enough that one that is not serving is not hammered.
    #[test]
    fn redial_backoff_is_bounded() {
        assert_eq!(retry_delay(1), std::time::Duration::from_millis(500));
        assert_eq!(retry_delay(4), std::time::Duration::from_millis(4000));
        assert_eq!(retry_delay(99), std::time::Duration::from_millis(4000));
    }

    /// Every endpoint this editor dials is derived from the one announced URL,
    /// so deriving must keep the pinned fingerprint: dropping it would turn a
    /// pinned dial into one that trusts nothing and fails.
    #[test]
    fn a_derived_path_keeps_the_pinned_fingerprint() {
        let fingerprint = "sha256:".to_owned() + &"ab".repeat(32);
        let root = Endpoint(format!("weida://{fingerprint}@127.0.0.1:7443/"));
        assert_eq!(
            root.path(FEED_PATH).expect("derive"),
            format!("weida://{fingerprint}@127.0.0.1:7443{FEED_PATH}")
        );
    }

    /// A malformed announcement is a runtime problem, not a panic here.
    #[test]
    fn a_bad_endpoint_reports_instead_of_panicking() {
        assert!(Endpoint("not a url".to_owned()).path(FEED_PATH).is_err());
    }
}
