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
  1150-1188).
- **Ordering across streams is `None`** (`docs/GUARANTEES.md:245`). Frames need a
  sequence number in the payload if a receiver must reject a stale one.
- **A receipt means the peer's transport holds the bytes**, nothing more
  (`docs/GUARANTEES.md:92-114`). Fine here: a dropped frame needs no ceremony.
- **Addresses are `weida://host:port/path`**, path opaque and matched exactly
  (`crates/core/src/addr.rs:15-28`, `crates/weida/src/listener.rs:48-57`). Topic
  prefix filtering exists only inside Pub/Sub
  (`crates/weida/src/pubsub.rs:29-35`).
- **TLS is mandatory and trust is explicit**: `ClientTls` carries trust anchors,
  `ServerTls` a chain and key, either from memory or file
  (`crates/weida/src/config.rs:36-119`). There is no authorization concept beyond
  certificate identity.
- **Integration**: workspace version 0.1.0, edition 2024, MSRV 1.88, needs an
  ambient Tokio reactor (`Cargo.toml:1-25`, `crates/weida/src/runtime.rs:42-64`).
  zeughaus is on rustc 1.97 with Tokio already present, so nothing conflicts.
  Whether the crates resolve from crates.io is unknown; a path or git dependency
  works either way.

## Proposal

### Frames are pulled, not pushed

The editor repaints at most a few dozen times per second and only ever wants the
newest frame. If it *asks* per repaint, "newest wins" is a property of the
protocol rather than a queue policy: there is no buffer to conflate, no drop
rule to tune, and a slow editor throttles itself by asking less often. That
sidesteps the two things weida does not have today (streaming fan-out and
conflation) and needs no new weida feature at all.

So: one weida **Req/Rep** endpoint on the runner, one exchange per sample.

```
Editor                                   Runner (zeughaus-runner)
  |  request  {node, pin, max_w, max_h, since_seq}   |
  |------------------------------------------------->|
  |  reply    {seq, w, h, format} + pixel bytes      |
  |<-------------------------------------------------|
```

- `max_w`/`max_h`: what the editor will actually draw. The runner downscales
  before sending, because it is the side that already holds the frame. A node
  body asks for its body size; a full-resolution inspection asks for 0 (meaning
  "no limit") and pays the 33 MB.
- `since_seq`: the sequence number the editor already has. Unchanged frame means
  an empty reply, so a static screen costs one small exchange per repaint rather
  than a frame.
- Reply body is streamed (`AsyncWrite`/`AsyncRead`), so nothing materializes on
  either side beyond the scaled frame itself.
- Failure is free: a lost or cancelled exchange means the editor draws the frame
  it already had and asks again next repaint. Dropping a `ReplyStream` resets the
  reply half (`crates/weida/src/transfer.rs:538-608`), which is exactly the
  behaviour wanted when a node scrolls out of view mid-transfer.

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

**Stage 1 -- pull, direct, no weida changes.** Runner binds a weida listener,
serves a `/samples` replier, publishes its address. Editor dials it, requests one
sample per visible frame node per repaint. Full-resolution inspection is the same
request with no size limit. This is implementable against weida as it stands
today.

**Stage 2 -- notification, still no weida changes.** Polling per repaint wastes
exchanges when nothing changes. A Pub/Sub topic per node carrying only
`{node_id, pin, seq}` -- tens of bytes, far below the 8 MiB cap -- lets an editor
request only after something moved. Pub/Sub is the right pattern here precisely
because these messages are tiny and losing one is harmless: the next repaint
catches up.

**Stage 3 -- push, when weida can stream fan-out.** With a streaming publisher, a
frame stream becomes a real subscription: the runner publishes, N editors consume,
and no one polls. This is the first stage that needs something weida does not
have, and it only pays off for the high-rate case (a video preview at display
rate), not for the editor's node bodies.

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
2. **A conflating queue** -- the planned `Coalescing(key)`. Only the newest sample
   per node-pin matters for monitoring, and a consumer that cannot keep up must
   fall behind in *time*, not in a growing backlog. Until it exists, pulling is
   the workaround, and pulling is good enough for the editor.
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
3. **Should the runner keep a short history per node?** Pull with `since_seq`
   assumes the newest frame is the only interesting one. Recording (stage 4) says
   otherwise, and where that history lives -- runner, broker, or capture store --
   is unresolved.
