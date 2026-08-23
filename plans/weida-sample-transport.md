# Sample transport over weida

## Context

Graph state syncs through SpacetimeDB: the editor writes it, `zeughaus-runner`
executes it, and scalar results come back as `node_output` rows. What has no path
at all is bulk sample data. A screen-capture node holds a 3840x2160 RGBA frame --
33 177 600 bytes -- and the editor that wants to draw it may be in another
process, on another machine, or several of both.

Frames deliberately do not travel through the store. This document proposes how
they travel over [weida](../../weida) instead, what weida already supports, and
what it would have to grow for the later stages.

Everything under "What weida provides today" was read from the weida source at
commit `2082a90`; file and line references are given so a reader can check rather
than trust. Everything under "Proposal" is design, not fact.

## What zeughaus needs

| Need | Shape | Volume |
| --- | --- | --- |
| Draw a node's frame in an editor | newest frame only, on repaint | 33 MB raw at 4K; ~0.5 MB at 480x270 |
| Draw a node's scalar values | last value per pin | bytes; already solved by `node_output` |
| Inspect a frame at full resolution | one frame, on demand | 33 MB, rare |
| Later: record a sample series | every frame, none skipped | unbounded |

Two numbers decide the design. A 4K RGBA frame at 60 Hz is 1.9 GB/s, which no
network link in this picture carries. The same frame downscaled to the ~480x270
a node body actually draws is 31 MB/s at 60 Hz, which a LAN carries comfortably.
So the transport never needs to move 4K frames continuously -- it needs to move
*what an editor will actually draw*, and full resolution only when someone asks
for it explicitly.

## What weida provides today

Verified, v0, layers L0 (raw QUIC streams) and L1 (Req/Rep, Push/Pull, Pub/Sub).

- **One transfer is one QUIC stream, and payload is never materialized.**
  `OutgoingTransfer` is an `AsyncWrite`, `IncomingTransfer` an `AsyncRead`
  (`crates/weida/src/transfer.rs:194-217,366-393`). The ignored test
  `large_stream_bounded_memory` streams 1 GiB each way and asserts peak RSS
  below 512 MiB (`crates/weida/tests/large.rs:132-148`). A 33 MB frame is
  unremarkable for this path.
- **Req/Rep is one bidirectional stream per exchange**, with the stream itself
  as the correlation: `Requester::open` returns
  `(OutgoingTransfer, ReplyStream)` (`crates/weida/src/endpoint.rs:134-140`), and
  the replying side can `take_body()` before `reply()` to read and write
  concurrently (`crates/weida/src/transfer.rs:458-526`).
- **Pub/Sub cannot carry a frame.** `Publisher::publish` takes a whole `Bytes`
  and returns `LimitExceeded` above `Limits::subscriber_buffer_bytes`, which
  defaults to 8 MiB (`crates/weida/src/endpoint.rs:328-350`,
  `crates/core/src/limits.rs:38-41,67`). Streaming fan-out is an explicit Phase 3
  deferral (`docs/IMPLEMENTATION.md:157-164`). The limit is configurable, but
  raising it to 64 MiB would buffer a 33 MB copy *per subscriber* inside the
  publisher -- the materialization the rest of the framework exists to avoid.
- **Nothing conflates.** Backpressure is `Block`, `Reject` or (fan-out only)
  `Drop`; "keep the newest, discard the rest" is not implemented and is planned
  as broker-level `Coalescing(key)` (`docs/GUARANTEES.md:181-190`, master plan
  1150-1188). The design below does not need it: a private stream per viewer
  conflates by construction, because the runtime holds only the newest frame.
- **Ordering across streams is `None`** (`docs/GUARANTEES.md:245`), but within one
  stream QUIC orders bytes, so a feed's frames arrive in the order written.
- **A receipt means the peer's transport holds the bytes**, nothing more
  (`docs/GUARANTEES.md:92-114`). Fine here: a dropped frame needs no ceremony.
- **Addresses are `weida://host:port/path`**, path opaque and matched exactly
  (`crates/core/src/addr.rs:15-28`, `crates/weida/src/listener.rs:48-57`).
- **TLS is mandatory and trust is explicit**: `ClientTls` carries trust anchors,
  `ServerTls` a chain and key, either from memory or file
  (`crates/weida/src/config.rs:36-119`). There is no authorization concept beyond
  certificate identity.
- **Integration**: workspace version 0.1.0, edition 2024, MSRV 1.88, needs an
  ambient Tokio reactor (`Cargo.toml:1-25`, `crates/weida/src/runtime.rs:42-64`).
  zeughaus is on rustc 1.97 with Tokio already present, so nothing conflicts.
## Proposal

### A video signal is one standing request, not a request per frame

A viewer states its terms once -- node, pin, the size it will draw, a frame-rate
ceiling -- and the runtime then writes frames on that one stream until the viewer
stops reading. One exchange per (viewer, node), private to that viewer.

This keeps what matters about pulling while paying for it once. The viewer still
decides the size and the rate and still cancels; what it no longer does is spend
a round trip per frame, which is bearable on a LAN and ruinous across the
internet. And because each viewer has its own stream, there is no fan-out and
therefore no 8 MiB publish cap and no missing conflation to work around:

- **Backpressure is QUIC's.** A slow viewer's stream fills, `write_all` blocks,
  and when it unblocks the runtime sends *what is current then* -- not the frame
  that was current when it started waiting. Conflation falls out of holding only
  the newest frame, which the capture path already does
  (`zeughaus-capture/src/screencast.rs` keeps a single-slot latest frame).
- **A dropped viewer costs nothing.** Reset the stream and the feed ends
  (`crates/weida/src/transfer.rs:538-608`).
- **Remote is not a special case.** The same request from the same code; only the
  address differs.

```
Viewer                                   Runtime (zeughaus-runner)
  |  FeedRequest {node, pin, w, h, fps}  (once, then FIN)
  |------------------------------------------------->|
  |  FrameHeader{seq,w,h} + RGBA, repeatedly         |
  |<-------------------------------------------------|
  |  reset when the node scrolls away / closes       |
```

### The runtime scales, and sizes snap to a ladder

The runtime holds the frame, so it is the side that can produce the ~0.5 MB a
node body draws instead of shipping 33 MB for it. Sizes snap to a tier ladder
(240/360/480/720/1080 lines, or source resolution above that), so two viewers
drawing the same node at 190 and 210 pixels tall both get the 240-line version:
the runtime scales once and sends the same bytes twice. Exact per-viewer sizes
would mean a separate scaling pass for a difference nobody can see at preview
size.

Scaling is box-averaged with a cap of 4x4 source samples per output pixel
(`zeughaus-samples`): point sampling turns text into noise at an 8:1 reduction,
and an uncapped box filter would read all 33 MB to produce a 480x270 preview --
the work this design exists to avoid.

Scalars stay in `node_output`. They are already there, they are the graph's
visible state rather than a sample, and moving them would buy latency nobody has
asked for.

### Discovery through the store

An editor already reconstructs the graph from SpacetimeDB. The same store is
therefore the natural place to answer "where do I reach the runtime": the
`runtime` table gains the address the runner listens on and the trust material a
client needs.

```rust
#[table(accessor = runtime, name = "runtime", public)]
pub struct Runtime {
    #[primary_key] identity: Identity,
    #[unique] #[auto_inc] seq: u64,
    sample_addr: String,   // "weida://10.0.0.8:7443/samples", empty if none
    sample_cert: String,   // PEM the client must trust
}
```

This answers the "broker or direct connection?" question for the first stage:
**direct**, because the store already tells every editor where the runtime is and
what to trust, and a direct QUIC connection is one hop with no third party to
operate. The broker becomes the answer when a hop is genuinely needed -- NAT
traversal, an editor that cannot reach the runner, many consumers of one stream,
or recording that must outlive the runner -- and it needs the same table entry
with the broker's address instead of the host's.

Certificate handling for stage 1: the runner generates a self-signed certificate
at startup and publishes it. That is honest for a LAN and for the same machine,
and it is the same trust model the store connection already has (an unauthenticated
local SpacetimeDB). Anything stronger belongs with the authorization question
below, not with the transport.

### Staging

**Stage 1 -- standing feeds, direct, no weida changes. Being built.** The runner
binds a weida listener, serves `/samples`, announces address and certificate in
the `runtime` row. An editor dials it and holds one feed per visible frame node
at the tier its body draws; full-resolution inspection is the same request with
no size limit. Implementable against weida exactly as it stands.

**Stage 2 -- shared scaling and rate discipline.** Same protocol, better
behaviour: one scaling pass shared by every viewer on a tier, a source that only
scales tiers somebody is watching, and per-feed rate ceilings so a 60 Hz source
does not force 60 Hz on a preview. Mostly runtime-side work.

**Stage 3 -- fan-out, when weida can stream it.** Today N viewers of one node
cost N streams and N identical writes. A streaming publisher would make it one
publish and N reads. Worth it when several viewers watch the same signal, which
is the collaboration case, not the single-user case.

**Stage 4 -- broker.** Two things move: a hop for editors that cannot reach the
runner, and a durable path for recorded sample series. Both are the broker's
purpose, neither is worth building into zeughaus first.

### What the broker's configuration model should mean here

Mapped onto the RabbitMQ-shaped split (the producer declares the queue, the
consumer declares its channel), zeughaus divides cleanly:

- The **runtime declares** one sample stream per node output that produces bulk
  data -- name, payload kind, and whether it is conflating (monitoring) or
  durable (recording). It is the only party that knows what a node produces.
- The **consumer declares** its own terms on attach: rate limit, maximum
  resolution, conflating or every-sample. Two editors watching the same node at
  different sizes are two channels on one queue, which is the distinction the
  split exists to express.

For monitoring, conflating is the correct default: a viewer that stalls should
resume at the current frame, never replay a backlog.

## What weida would need, in priority order

1. **Streaming fan-out** -- a publisher that hands out a stream per subscriber
   instead of a whole `Bytes` (`Publisher::open(topic) -> OutgoingTransfer`, or
   equivalent). Without it, push-based frames are impossible at any size above
   `subscriber_buffer_bytes`, and raising that limit trades the problem for a
   per-subscriber 33 MB copy. Already a recorded Phase 3 deferral.
2. **A conflating queue** -- the planned `Coalescing(key)`. Not needed for a
   private stream per viewer (the runtime holds only the newest frame, so the
   conflation is free), but the moment fan-out exists it does: one publisher and
   N consumers at different speeds is exactly where "keep the newest, discard the
   rest" has to live in the transport rather than in the producer.
3. **Peer authorization beyond certificate trust.** Stage 1 is a LAN with a
   self-signed certificate; a user who reaches a SpacetimeDB over the internet and
   wants monitoring needs something that says *which* client may attach. If weida
   grows a credential in the handshake, zeughaus would pass the session token it
   already has. If not, an application handshake on a `/auth` path is
   zeughaus-side work -- so this is a question, not a blocker.
4. **Per-topic drop counters.** `Publisher::dropped` is aggregate
   (`crates/weida/src/endpoint.rs:366-373`); knowing *which* sample channel is
   starving is what makes a stall diagnosable.
5. **A stated dependency form** -- crates.io release or a git tag zeughaus can pin.
   MSRV 1.88 and Tokio are already satisfied here.

Nothing above blocks stage 1. Items 1 and 2 are what turn stage 3 from a
workaround into the natural shape.

## Rejected alternatives

- **Frames through SpacetimeDB.** A state store is the wrong pipe: 33 MB per
  frame, and every row is replicated to every subscriber whether they draw it or
  not. Already decided against; this document exists because of it.
- **Pub/Sub with a raised `subscriber_buffer_bytes`.** Buys a 33 MB buffered copy
  per subscriber inside the publisher and still cannot stream. It converts a
  missing feature into a memory problem.
- **A hand-rolled TCP or WebSocket side channel.** It would duplicate what weida
  is for -- and specifically the part that is hardest to get right, streaming
  without materialization under backpressure and cancellation.
- **Encoding frames as JPEG or H.264 for stage 1.** Downscaling to the size the
  editor draws already reduces 33 MB to ~0.5 MB with no codec, no dependency and
  no latency. Compression is worth revisiting when the remote case is real.

## Open questions

1. **Where does the runner's certificate come from?** Self-signed per start is
   fine on a LAN and needs no configuration. A shared CA would let editors trust
   a runtime they have not met. Decision belongs with the authorization question.
2. **Does an editor ever need the unscaled frame more than occasionally?** If
   full-resolution monitoring at rate becomes a real workflow, compression stops
   being optional and stage 3 arrives earlier.
3. **Should the runtime keep a short history per node?** A live feed assumes the
   newest frame is the only interesting one. Recording says otherwise, and where
   that history lives -- runtime, broker, or capture store -- is unresolved.
4. **Is one tier per node right, or one per viewer?** Sharing a scaling pass
   argues for tiers; a viewer that wants an exact size argues against. The ladder
   is the current answer and it is cheap to revisit -- `FeedRequest` already
   carries the exact size the viewer asked for.
