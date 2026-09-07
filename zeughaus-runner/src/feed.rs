//! Serving the sample feed: how a frame leaves this process.
//!
//! Everything the graph computes reaches an editor through SpacetimeDB except
//! frames. A 3840x2160 RGBA frame is 33 177 600 bytes, so it travels over its
//! own QUIC connection instead ([`weida`]), and this module is the serving end
//! of the protocol that [`zeughaus_samples`] defines.
//!
//! Three pieces, in the order a frame passes through them:
//!
//! * [`FrameRegistry`] -- the executing thread publishes the newest frame per
//!   output pin here after each pass. Exactly one frame per pin: a viewer wants
//!   the current picture, and a backlog of 33 MB frames is how a process runs
//!   out of memory. Frames are shared, never copied; [`zeughaus_core::Image`]
//!   is `Arc`-backed and the registry holds a refcount, not a buffer.
//! * [`ScaleCache`] -- one scaled result per (pin, tier, source frame), shared
//!   by every viewer. That sharing is the whole reason the tier ladder exists.
//! * [`accept_feeds`] -- the `/samples` accept loop: one long-lived exchange
//!   per viewer, a [`FeedRequest`] in and headed frames out until the viewer
//!   stops.
//!
//! The registry is deliberately *authoritative* rather than additive. Each pass
//! states the full set of frame pins the graph has, so a pin that vanished is
//! dropped -- which both releases its frame and is how its viewers learn the
//! feed is over. Without that, a removed node's last frame and the task writing
//! it would both outlive the node.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::watch;
use weida::{IncomingRequest, OutgoingTransfer, Replier, TransferMeta};
use zeughaus_core::{Image, NodeId};
use zeughaus_samples::{FeedRequest, FrameHeader, MAX_DIMENSION, ladder, scale_to_fit};

/// Largest [`FeedRequest`] this server will read.
///
/// A request is a node id, a pin name and four numbers -- a few hundred bytes.
/// The cap is what stops a viewer from making the runtime allocate on its
/// behalf: over it, the rest of the payload is refused without ever being
/// buffered.
const MAX_REQUEST_BYTES: usize = 4096;

/// Scaled frames kept across viewers.
///
/// Small on purpose: entries hold frames. Eight covers every tier twice over,
/// and a new source frame invalidates by key rather than by eviction, so the
/// cache turns over on its own.
const SCALE_CACHE_ENTRIES: usize = 8;

// --- the registry -------------------------------------------------------------

/// The newest frame per frame-producing output pin.
///
/// Shared between the thread that executes the graph (one writer) and the tasks
/// that serve viewers (many readers). A `std::sync::Mutex` rather than an async
/// one because every critical section here is a hash lookup and an `Arc`
/// refcount, and the writer is not async at all.
pub struct FrameRegistry {
    slots: Mutex<Slots>,
    /// Bumped whenever a pin gained a frame or disappeared. Feeds park on this
    /// instead of polling. A `watch` rather than a `Notify` because it
    /// remembers an update that landed while a feed was busy writing, which is
    /// precisely the case a slow viewer is in.
    revision: watch::Sender<u64>,
}

#[derive(Default)]
struct Slots {
    pins: HashMap<NodeId, HashMap<Arc<str>, Slot>>,
    /// The pass currently being published. Stamped into every slot the pass
    /// mentions, so the ones it did not mention can be dropped afterwards
    /// without building a second set of keys.
    pass: u64,
    /// Stamped on the next distinct frame. One counter for the whole registry:
    /// it is monotonic per pin either way, and a single number also serves as
    /// the revision feeds wait on.
    next_seq: u64,
}

struct Slot {
    pass: u64,
    /// `None` until the node has produced its first frame. The pin still has to
    /// appear: absence means "not served", and a viewer that attached before
    /// the first capture has to wait rather than be told the feed is over.
    frame: Option<(u64, Image)>,
}

/// What a pin has for a viewer right now.
enum Lookup {
    /// The newest frame and its source sequence number.
    Frame(u64, Image),
    /// The pin is served but has produced nothing yet.
    Pending,
    /// Not served: the node is gone, the pin no longer produces frames, or this
    /// process no longer executes the graph.
    Gone,
}

/// What to do next for one viewer.
enum Next {
    /// Send this frame.
    Send(u64, Image),
    /// Nothing this viewer has not already seen; wait for the registry to move.
    Wait,
    /// The feed is over.
    Gone,
}

impl FrameRegistry {
    pub fn new() -> FrameRegistry {
        FrameRegistry {
            slots: Mutex::new(Slots::default()),
            revision: watch::channel(0).0,
        }
    }

    /// Replaces the registry's contents with this pass's frame pins.
    ///
    /// Authoritative: `pins` is the complete set of frame-producing output pins
    /// the graph has, with the frame each currently holds. A pin left out is
    /// dropped, releasing its frame and ending its feeds.
    ///
    /// A pin whose image is the *same buffer* as last pass keeps its sequence
    /// number. The node did not run, so this is the frame every viewer already
    /// has, and re-stamping it would make all of them resend it -- which is
    /// what makes "never send the same frame twice" free rather than a
    /// per-viewer comparison of 33 MB.
    pub fn publish<'a>(&self, pins: impl IntoIterator<Item = (NodeId, &'a str, Option<&'a Image>)>) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Slots {
            pins: stored,
            pass,
            next_seq,
        } = &mut *slots;
        *pass += 1;
        let pass = *pass;
        let mut moved = false;

        for (node, pin, image) in pins {
            let by_pin = stored.entry(node).or_default();
            match by_pin.get_mut(pin) {
                Some(slot) => {
                    slot.pass = pass;
                    if !same_buffer(slot.frame.as_ref().map(|(_, held)| held), image) {
                        *next_seq += 1;
                        slot.frame = image.map(|i| (*next_seq, i.clone()));
                        moved = true;
                    }
                }
                None => {
                    let frame = image.map(|i| {
                        *next_seq += 1;
                        (*next_seq, i.clone())
                    });
                    moved |= frame.is_some();
                    by_pin.insert(Arc::from(pin), Slot { pass, frame });
                }
            }
        }

        // Anything this pass did not mention is no longer produced.
        let before = stored.len();
        stored.retain(|_, by_pin| {
            let kept = by_pin.len();
            by_pin.retain(|_, slot| slot.pass == pass);
            moved |= by_pin.len() != kept;
            !by_pin.is_empty()
        });
        moved |= stored.len() != before;

        drop(slots);
        if moved {
            self.wake();
        }
    }

    /// Drops everything.
    ///
    /// Used when this process stops owning execution: the frames it holds are
    /// the last ones it produced, another runtime is producing the real ones
    /// now, and every viewer here has to be sent away rather than shown a
    /// picture that has stopped moving.
    pub fn clear(&self) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.pins.is_empty() {
            return;
        }
        slots.pins.clear();
        drop(slots);
        self.wake();
    }

    /// Signals every waiting feed. The value carried is the sequence counter,
    /// which is useful in a debugger and ignored by the feeds -- `watch` marks
    /// a send as a change regardless of the value, so an equal number still
    /// wakes them.
    fn wake(&self) {
        let seq = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq;
        let _ = self.revision.send(seq);
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    fn lookup(&self, node: NodeId, pin: &str) -> Lookup {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        match slots.pins.get(&node).and_then(|by_pin| by_pin.get(pin)) {
            // Cloning an `Image` is an `Arc` bump and two `u32`s, so the frame
            // leaves the lock without the buffer being touched.
            Some(Slot {
                frame: Some((seq, image)),
                ..
            }) => Lookup::Frame(*seq, image.clone()),
            Some(_) => Lookup::Pending,
            None => Lookup::Gone,
        }
    }

    /// What this viewer should do next, given the last sequence number it was
    /// sent.
    ///
    /// The one place the "never send the same source frame twice" rule lives:
    /// a viewer that already has this sequence number waits, however many
    /// passes the graph runs meanwhile.
    fn next_for(&self, node: NodeId, pin: &str, sent: Option<u64>) -> Next {
        match self.lookup(node, pin) {
            Lookup::Frame(seq, _) if Some(seq) == sent => Next::Wait,
            Lookup::Frame(seq, image) => Next::Send(seq, image),
            Lookup::Pending => Next::Wait,
            Lookup::Gone => Next::Gone,
        }
    }
}

impl Default for FrameRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether these two slots hold the same pixels, by buffer identity.
///
/// Pointer equality, not comparison: a node that did not run hands back the
/// very `Arc` from its previous execution, and a node that did run allocated a
/// new one. Comparing 33 MB to answer the same question would cost more than
/// sending the frame.
fn same_buffer(held: Option<&Image>, incoming: Option<&Image>) -> bool {
    match (held, incoming) {
        (Some(a), Some(b)) => Arc::ptr_eq(a.rgba(), b.rgba()),
        (None, None) => true,
        _ => false,
    }
}

// --- the scale cache ----------------------------------------------------------

/// Identifies one scaled frame.
///
/// The source sequence number is part of the key, so a new frame invalidates
/// the old scaling by simply not matching it -- there is no staleness to
/// detect and nothing to invalidate explicitly.
#[derive(Clone, PartialEq, Eq)]
struct ScaleKey {
    node: NodeId,
    pin: Arc<str>,
    /// `None` is source resolution.
    tier: Option<u32>,
    seq: u64,
}

/// Scaled frames, shared by every feed.
///
/// This is what the tier ladder buys: two viewers drawing the same node at 190
/// and 210 pixels tall both ask for tier 240, and the second one finds the
/// first one's result instead of box-filtering 33 MB again.
pub struct ScaleCache {
    entries: Mutex<Vec<(ScaleKey, Image)>>,
}

impl ScaleCache {
    pub fn new() -> ScaleCache {
        ScaleCache {
            entries: Mutex::new(Vec::with_capacity(SCALE_CACHE_ENTRIES)),
        }
    }

    /// The scaled frame for `key`, scaling `source` if nobody has yet.
    ///
    /// The scale happens with the lock held. That serializes two viewers on
    /// different tiers by one scaling pass, and it is still the cheaper trade:
    /// releasing the lock would let two viewers on the *same* tier -- the case
    /// the ladder exists for -- each do the work, and a viewer waiting for the
    /// lock waits no longer than it would have spent scaling itself.
    fn scaled(&self, key: &ScaleKey, source: &Image) -> Image {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, image)) = entries.iter().find(|(k, _)| k == key) {
            return image.clone();
        }
        let scaled = match key.tier {
            // Bound the height only: the ladder is heights, the aspect ratio
            // comes from the source, and a width bound would make two viewers
            // of the same tier miss each other's result.
            Some(tier) => scale_to_fit(source, MAX_DIMENSION, tier),
            // Source resolution. `scale_to_fit` would return the same clone;
            // saying so directly keeps the "no limit" case obviously free.
            None => source.clone(),
        };
        if entries.len() == SCALE_CACHE_ENTRIES {
            // Oldest out. Insertion order is close enough to usefulness here:
            // entries die of a new source sequence long before they age out.
            entries.remove(0);
        }
        entries.push((key.clone(), scaled.clone()));
        scaled
    }
}

impl Default for ScaleCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Most feeds this process serves at the same time.
///
/// Every feed is a task, a watch receiver, a QUIC stream and -- while it is
/// writing -- a scaled frame. A peer can open them as fast as it can dial, so
/// without a ceiling "how much memory does the runtime use" is a number a
/// viewer chooses. Sixty-four is far above any real editor: a Display node is
/// one feed, and viewers sharing a pin share one.
const MAX_FEEDS: usize = 64;

/// Whether one more feed can be served, and whether refusing it is news.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Serve,
    Refuse {
        /// Only the first refusal of an episode is logged: a peer that keeps
        /// dialling would otherwise write the log, which is precisely the
        /// resource this limit exists to protect.
        report: bool,
    },
}

/// The capacity rule, and which peers it has already told.
#[derive(Default)]
struct Capacity {
    told: HashSet<String>,
}

impl Capacity {
    /// Answers one request, given how many feeds are being served.
    ///
    /// Serving resets the memory of who has been told, so "full again" is a
    /// new episode worth one line per peer rather than silence forever.
    fn offer(&mut self, serving: usize, peer: &str) -> Admission {
        if serving < MAX_FEEDS {
            self.told.clear();
            return Admission::Serve;
        }
        Admission::Refuse {
            report: self.told.insert(peer.to_string()),
        }
    }
}

/// One served feed's claim on [`MAX_FEEDS`], released when its task ends.
///
/// A count rather than a semaphore permit because nothing here ever waits for
/// capacity -- it refuses -- and one shared counter keeps the decision in
/// [`Capacity::offer`], where it can be read and tested.
struct FeedSlot(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for FeedSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How a peer is named in the log: the identity it proved in the handshake, or
/// that it proved none.
///
/// The fingerprint is the only trustworthy name a peer has -- it comes from
/// the TLS handshake rather than from anything the peer wrote. Viewers dial
/// anonymously today, so this is usually the same name for all of them; that
/// is also why the limit is global and not per peer. A per-peer share needs an
/// identity to divide by, and this is where it would come from.
fn peer_name(meta: &weida::IncomingMeta) -> String {
    match meta.peer {
        Some(fingerprint) => fingerprint.to_string(),
        None => "an anonymous viewer".to_string(),
    }
}

/// Accepts feeds until the replier goes away, which for this process means
/// never: it owns the replier for the life of the program.
///
/// A request beyond [`MAX_FEEDS`] is dropped without a reply, which is what
/// tells the viewer no reply is coming (weida has no public typed refusal on a
/// reply half). Its `frames` loop then backs off and redials, so a viewer that
/// arrives during a burst gets its feed a moment later instead of being told
/// nothing at all.
pub async fn accept_feeds(replier: Replier, frames: Arc<FrameRegistry>) {
    let cache = Arc::new(ScaleCache::new());
    let serving = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut capacity = Capacity::default();
    loop {
        match replier.accept().await {
            Ok(request) => {
                let in_flight = serving.load(std::sync::atomic::Ordering::Relaxed);
                match capacity.offer(in_flight, &peer_name(request.meta())) {
                    Admission::Serve => {
                        serving.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(serve_feed(
                            request,
                            Arc::clone(&frames),
                            Arc::clone(&cache),
                            FeedSlot(Arc::clone(&serving)),
                        ));
                    }
                    Admission::Refuse { report } => {
                        if report {
                            eprintln!(
                                "[feed] serving {in_flight} feeds already, refusing {}",
                                peer_name(request.meta())
                            );
                        }
                        drop(request);
                    }
                }
            }
            Err(e) => {
                eprintln!("[feed] stopped accepting: {e}");
                return;
            }
        }
    }
}

/// Why a feed stopped.
enum FeedEnd {
    /// The viewer reset the exchange, dropped its reply stream or went away.
    Viewer,
    /// The pin stopped being served: its node was removed, or this process no
    /// longer executes the graph.
    Gone,
}

impl std::fmt::Display for FeedEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedEnd::Viewer => f.write_str("viewer left"),
            FeedEnd::Gone => f.write_str("no longer served"),
        }
    }
}

/// Serves one viewer for as long as it wants frames.
///
/// `_slot` is this feed's claim on [`MAX_FEEDS`]: it is released when this
/// task ends, however it ends, which is why it is owned here rather than
/// counted down by the accept loop.
async fn serve_feed(
    mut request: IncomingRequest,
    frames: Arc<FrameRegistry>,
    cache: Arc<ScaleCache>,
    _slot: FeedSlot,
) {
    let mut body = request.take_body();
    // Taken before `reply` consumes the request. This is the only prompt signal
    // that a viewer walked away, and it has to sit beside a `write_all` that
    // may be parked on flow control -- and beside the wait of a feed whose
    // source has not moved for minutes, which would otherwise hold a task and a
    // frame alive forever.
    let canceled: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(request.canceled());

    let encoded = match body.read_capped(MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("[feed] refused a request: {e}");
            return;
        }
    };
    // Dropping the request without replying tells the viewer no reply is
    // coming, which is the honest answer to bytes that are not a request.
    let Some(feed) = FeedRequest::decode(&encoded) else {
        eprintln!("[feed] refused a malformed request ({} bytes)", encoded.len());
        return;
    };

    let node = NodeId(feed.node_id);
    let tier = ladder::tier_for(feed.height);
    let label = format!("{node} {} at {}", feed.pin, describe_tier(tier));

    let mut reply = match request.reply(TransferMeta::default()).await {
        Ok(reply) => reply,
        Err(e) => {
            eprintln!("[feed] {label}: cannot open the reply half: {e}");
            return;
        }
    };

    eprintln!("[feed] {label}: started");
    let (sent, end) = stream_frames(&mut reply, canceled, &frames, &cache, &feed, tier).await;
    // `finish` on a feed the viewer abandoned fails, and that is not worth
    // reporting: the reason it ended is already the line below.
    let _ = reply.finish();
    eprintln!("[feed] {label}: ended after {sent} frame(s), {end}");
}

/// Writes frames until the feed ends. Returns how many went out and why it
/// stopped.
async fn stream_frames(
    reply: &mut OutgoingTransfer,
    mut canceled: Pin<Box<dyn Future<Output = ()> + Send>>,
    frames: &FrameRegistry,
    cache: &ScaleCache,
    feed: &FeedRequest,
    tier: Option<u32>,
) -> (u64, FeedEnd) {
    let node = NodeId(feed.node_id);
    let pin: Arc<str> = Arc::from(&*feed.pin);
    let interval = feed.frame_interval();

    let mut revision = frames.subscribe();
    let mut sent: Option<u64> = None;
    let mut count = 0u64;
    let mut next_due = Instant::now();

    loop {
        // Rate limit before choosing a frame, not after: a frame picked and
        // then slept on is a frame the sleep made stale.
        if let Some(interval) = interval {
            let now = Instant::now();
            if next_due > now {
                tokio::select! {
                    () = &mut canceled => return (count, FeedEnd::Viewer),
                    () = tokio::time::sleep(next_due - now) => {}
                }
            }
            next_due = Instant::now() + interval;
        }

        // Mark the current revision seen *before* looking, so an update landing
        // between the lookup and the wait below is not slept through.
        revision.borrow_and_update();
        let (seq, source) = match frames.next_for(node, &pin, sent) {
            Next::Send(seq, image) => (seq, image),
            Next::Gone => return (count, FeedEnd::Gone),
            Next::Wait => {
                tokio::select! {
                    () = &mut canceled => return (count, FeedEnd::Viewer),
                    changed = revision.changed() => {
                        // The registry is owned by this process for its whole
                        // life, so the sender cannot drop; treat it as the feed
                        // ending rather than assume.
                        if changed.is_err() {
                            return (count, FeedEnd::Gone);
                        }
                    }
                }
                continue;
            }
        };

        let scaled = cache.scaled(
            &ScaleKey {
                node,
                pin: Arc::clone(&pin),
                tier,
                seq,
            },
            &source,
        );
        // Release the source before writing: a feed holds one frame at a time,
        // and on the unscaled path `scaled` is that same buffer anyway.
        drop(source);

        let header = FrameHeader::new(seq, scaled.width(), scaled.height());
        let write = async {
            // Two writes rather than one concatenated buffer: QUIC orders the
            // stream, so the header precedes the pixels without copying half a
            // megabyte to put them together.
            reply.write_all(&header.encode()).await?;
            reply.write_all(scaled.rgba()).await
        };
        tokio::select! {
            () = &mut canceled => return (count, FeedEnd::Viewer),
            result = write => {
                if result.is_err() {
                    return (count, FeedEnd::Viewer);
                }
            }
        }
        sent = Some(seq);
        count += 1;
        // Nothing is remembered about this frame beyond its sequence number.
        // The next pass round re-reads the registry, so a viewer that spent a
        // second parked on flow control resumes at the current frame instead of
        // the one that was current when it started waiting.
    }
}

fn describe_tier(tier: Option<u32>) -> String {
    match tier {
        Some(height) => format!("{height}p"),
        None => "source".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame whose pixels are all `fill`, so a scaled result can be checked
    /// by value as well as by size.
    fn frame(width: u32, height: u32, fill: u8) -> Image {
        Image::from_rgba(
            width,
            height,
            vec![fill; width as usize * height as usize * 4],
        )
    }

    fn key(pin: &str, tier: Option<u32>, seq: u64) -> ScaleKey {
        ScaleKey {
            node: NodeId(1),
            pin: Arc::from(pin),
            tier,
            seq,
        }
    }

    #[test]
    fn a_pin_keeps_only_its_newest_frame() {
        let registry = FrameRegistry::new();
        let first = frame(4, 4, 1);
        let second = frame(4, 4, 2);

        registry.publish([(NodeId(7), "frame", Some(&first))]);
        let Lookup::Frame(first_seq, held) = registry.lookup(NodeId(7), "frame") else {
            panic!("the pin must hold the frame it was published with");
        };
        assert!(Arc::ptr_eq(held.rgba(), first.rgba()));
        // A viewer's handle on the old frame is what keeps it alive, so let go
        // of it before asking whether the registry did.
        drop(held);

        registry.publish([(NodeId(7), "frame", Some(&second))]);
        let Lookup::Frame(second_seq, held) = registry.lookup(NodeId(7), "frame") else {
            panic!("the pin must hold the newer frame");
        };
        assert!(Arc::ptr_eq(held.rgba(), second.rgba()));
        assert!(second_seq > first_seq, "sequence numbers must advance");
        // One slot per pin: the older frame is not reachable at all, which is
        // what keeps a backlog of 33 MB frames from accumulating. `first` is the
        // only owner left.
        assert_eq!(
            Arc::strong_count(first.rgba()),
            1,
            "the previous frame must be released"
        );
    }

    #[test]
    fn an_unchanged_frame_keeps_its_sequence_number() {
        let registry = FrameRegistry::new();
        let image = frame(4, 4, 1);

        registry.publish([(NodeId(7), "frame", Some(&image))]);
        let Lookup::Frame(first, _) = registry.lookup(NodeId(7), "frame") else {
            panic!("published frame missing");
        };
        // The node did not run, so the executor hands back the same buffer.
        registry.publish([(NodeId(7), "frame", Some(&image))]);
        let Lookup::Frame(second, _) = registry.lookup(NodeId(7), "frame") else {
            panic!("published frame missing");
        };
        assert_eq!(first, second, "a pass that changed nothing must not restamp");
    }

    #[test]
    fn a_pin_the_pass_omits_is_no_longer_served() {
        let registry = FrameRegistry::new();
        let image = frame(4, 4, 1);

        registry.publish([(NodeId(7), "frame", Some(&image))]);
        // The node was removed, so this pass does not mention it.
        registry.publish(std::iter::empty());
        assert!(matches!(registry.lookup(NodeId(7), "frame"), Lookup::Gone));
    }

    #[test]
    fn a_pin_without_a_frame_yet_is_pending_not_gone() {
        let registry = FrameRegistry::new();
        // The node exists and declares a frame pin, but has not run.
        registry.publish([(NodeId(7), "frame", None)]);
        assert!(matches!(
            registry.lookup(NodeId(7), "frame"),
            Lookup::Pending
        ));
        assert!(matches!(
            registry.next_for(NodeId(7), "frame", None),
            Next::Wait
        ));
    }

    #[test]
    fn losing_ownership_ends_every_feed() {
        let registry = FrameRegistry::new();
        let image = frame(4, 4, 1);
        registry.publish([(NodeId(7), "frame", Some(&image))]);
        registry.clear();
        assert!(matches!(
            registry.next_for(NodeId(7), "frame", None),
            Next::Gone
        ));
    }

    #[test]
    fn the_same_source_sequence_is_never_sent_twice() {
        let registry = FrameRegistry::new();
        let image = frame(4, 4, 1);
        registry.publish([(NodeId(7), "frame", Some(&image))]);

        let Next::Send(seq, _) = registry.next_for(NodeId(7), "frame", None) else {
            panic!("a viewer with nothing yet must be sent the current frame");
        };
        // Same viewer, same frame: it already has it.
        assert!(matches!(
            registry.next_for(NodeId(7), "frame", Some(seq)),
            Next::Wait
        ));
        // Passes that change nothing do not change that.
        registry.publish([(NodeId(7), "frame", Some(&image))]);
        assert!(matches!(
            registry.next_for(NodeId(7), "frame", Some(seq)),
            Next::Wait
        ));
        // A new frame does.
        let next = frame(4, 4, 2);
        registry.publish([(NodeId(7), "frame", Some(&next))]);
        assert!(matches!(
            registry.next_for(NodeId(7), "frame", Some(seq)),
            Next::Send(_, _)
        ));
    }

    #[test]
    fn a_viewer_that_missed_frames_resumes_at_the_current_one() {
        let registry = FrameRegistry::new();
        let first = frame(4, 4, 1);
        registry.publish([(NodeId(7), "frame", Some(&first))]);
        let Next::Send(seen, _) = registry.next_for(NodeId(7), "frame", None) else {
            panic!("frame expected");
        };

        // Three passes while the viewer was parked on flow control.
        let mut newest = None;
        for fill in 2..=4u8 {
            let image = frame(4, 4, fill);
            registry.publish([(NodeId(7), "frame", Some(&image))]);
            newest = Some(image);
        }
        let newest = newest.expect("three passes ran");

        let Next::Send(_, resumed) = registry.next_for(NodeId(7), "frame", Some(seen)) else {
            panic!("the viewer must be handed a frame");
        };
        // Not the one that was current when it started waiting.
        assert!(Arc::ptr_eq(resumed.rgba(), newest.rgba()));
    }

    #[test]
    fn a_requested_height_snaps_to_a_tier_before_scaling() {
        let cache = ScaleCache::new();
        let source = frame(3840, 2160, 0x40);

        // 270 lines is between tiers, so it is served at 360 -- and the aspect
        // ratio comes from the source, not from the request.
        let tier = ladder::tier_for(270);
        assert_eq!(tier, Some(360));
        let scaled = cache.scaled(&key("frame", tier, 1), &source);
        assert_eq!((scaled.width(), scaled.height()), (640, 360));
        // Box-averaging a uniform frame must reproduce it exactly; a scaler
        // that read outside the frame would show up here as an edge artifact.
        assert!(scaled.rgba().iter().all(|b| *b == 0x40));

        // Above the ladder there is no tier and the frame goes out untouched,
        // sharing the source buffer rather than copying 33 MB.
        let unscaled = cache.scaled(&key("frame", ladder::tier_for(2160), 1), &source);
        assert!(Arc::ptr_eq(unscaled.rgba(), source.rgba()));
    }

    #[test]
    fn two_viewers_on_one_tier_share_a_single_scaling_pass() {
        let cache = ScaleCache::new();
        let source = frame(1920, 1080, 0x11);

        // 190 and 210 pixels tall: two different requests, one tier.
        let one = ladder::tier_for(190);
        let two = ladder::tier_for(210);
        assert_eq!(one, two);

        let first = cache.scaled(&key("frame", one, 5), &source);
        let second = cache.scaled(&key("frame", two, 5), &source);
        // The same buffer, so the second viewer paid for no scaling at all.
        assert!(Arc::ptr_eq(first.rgba(), second.rgba()));

        // A different tier is different work.
        let bigger = cache.scaled(&key("frame", ladder::tier_for(700), 5), &source);
        assert!(!Arc::ptr_eq(first.rgba(), bigger.rgba()));
        // So is the same tier on a newer source frame.
        let newer = cache.scaled(&key("frame", one, 6), &source);
        assert!(!Arc::ptr_eq(first.rgba(), newer.rgba()));
        // And so is another pin, even at the same tier and sequence number.
        let other_pin = cache.scaled(&key("mask", one, 5), &source);
        assert!(!Arc::ptr_eq(first.rgba(), other_pin.rgba()));
    }

    #[test]
    fn the_scale_cache_stays_bounded() {
        let cache = ScaleCache::new();
        let source = frame(64, 64, 1);
        for seq in 0..(SCALE_CACHE_ENTRIES as u64 * 3) {
            cache.scaled(&key("frame", Some(32), seq), &source);
        }
        assert_eq!(
            cache.entries.lock().expect("cache").len(),
            SCALE_CACHE_ENTRIES
        );
    }

    /// Below the ceiling everything is served; at it, nothing is -- and the
    /// refusal is said once per peer, because a peer that keeps dialling must
    /// not be able to write the log either.
    #[test]
    fn a_full_server_refuses_and_says_so_once() {
        let mut capacity = Capacity::default();
        assert_eq!(capacity.offer(0, "viewer-a"), Admission::Serve);
        assert_eq!(capacity.offer(MAX_FEEDS - 1, "viewer-a"), Admission::Serve);

        assert_eq!(
            capacity.offer(MAX_FEEDS, "viewer-a"),
            Admission::Refuse { report: true }
        );
        assert_eq!(
            capacity.offer(MAX_FEEDS, "viewer-a"),
            Admission::Refuse { report: false },
            "the same peer is told once"
        );
        assert_eq!(
            capacity.offer(MAX_FEEDS + 5, "viewer-b"),
            Admission::Refuse { report: true },
            "another peer is a different report"
        );
    }

    /// Capacity coming back ends the episode: the next time the server fills
    /// up, that is worth saying again rather than staying silent forever.
    #[test]
    fn capacity_returning_starts_a_new_episode() {
        let mut capacity = Capacity::default();
        assert_eq!(
            capacity.offer(MAX_FEEDS, "viewer-a"),
            Admission::Refuse { report: true }
        );
        assert_eq!(capacity.offer(MAX_FEEDS - 1, "viewer-a"), Admission::Serve);
        assert_eq!(
            capacity.offer(MAX_FEEDS, "viewer-a"),
            Admission::Refuse { report: true }
        );
    }

    /// A feed's claim is released when its task ends, however it ends.
    #[test]
    fn a_slot_is_returned_when_it_is_dropped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let serving = Arc::new(AtomicUsize::new(0));
        serving.fetch_add(1, Ordering::Relaxed);
        let slot = FeedSlot(Arc::clone(&serving));
        assert_eq!(serving.load(Ordering::Relaxed), 1);
        drop(slot);
        assert_eq!(serving.load(Ordering::Relaxed), 0);
    }
}
