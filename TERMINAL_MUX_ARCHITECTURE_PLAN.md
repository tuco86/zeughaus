# Terminal Mux Architecture Plan

## Summary

Build a runner-owned terminal multiplexer inspired by WezTerm's architecture, without importing WezTerm's mux, client, codec, GUI, SSH, Lua, or global configuration stack.

Direct reuse boundary:

- `portable-pty` owns cross-platform PTY/process creation and resize.
- A pinned `wezterm-term` revision owns escape parsing, terminal modes, canonical screen state, stable rows, scrollback, cursor, and changed-row sequence numbers.
- Zeughaus owns workspace topology, wire types, QUIC exchanges, authorization, reconnect behavior, client caches, iced input, and GPU rendering.

Weida is sufficient as it is. This plan needs only capabilities it already has: one pooled QUIC connection for one stable path, concurrent bidirectional exchanges, stream cancellation, bounded flow control, peer identity, and mTLS. It does not require a Weida change. Warm attaches reuse the live connection and pay no TLS/QUIC handshake. A real connection loss performs Weida's current fresh QUIC/TLS/HELLO negotiation followed by one compact application attach; the implementation and documentation must not call that 0-RTT or session resumption. Weida decision 0031 (transparent redial, `ReconnectPolicy`, `PeerEvent`, sender outbox; Weida backlog B-270 to B-273) moves the redial loop into Weida once it lands; until then Zeughaus owns it, and either way Zeughaus owns everything above the transport: epoch, topology reconciliation, fresh snapshots.

The first production cut is the selected text-terminal scope: ANSI/DEC behavior, Unicode, alternate screen, colors/styles, keyboard/text/IME, paste, mouse, resize/rewrap, selection, copy, bounded scrollback, reconnect, and multi-client viewing/control. Kitty/Sixel/iTerm image transfer is excluded from this cut, but the DTO and renderer layering must reserve resource references so adding images does not redesign rows or GPU ordering.

## Grounded findings

- WezTerm keeps the child process, PTY, parser, canonical terminal, scrollback, and mux topology on the server. Its client receives full topology snapshots, per-client sequence-based row damage, cursor/metadata changes, and lazy scrollback rows.
- WezTerm's useful network ideas are independent per-pane subscriber cursors, visible changed rows inline with deltas, cursor-row inclusion, stable-row scrollback paging, client cache generations, input serial reconciliation, and full mark/sweep topology reconciliation after reconnect.
- WezTerm's mux/server/client/codec crates are tightly coupled to global `Mux`, configuration/Lua, smol/promise scheduling, concrete internal types, TCP/TLS, and SSH. Depending on them would import a second application architecture.
- `wezterm-term::Terminal::advance_bytes` and the screen changed-row APIs provide the canonical model and damage tracking needed by the runner. `portable-pty` supplies the correct platform process boundary.
- Weida's QUIC pool key includes the endpoint path. One `/mux` path therefore yields one reused connection; per-terminal paths or separate control/event paths would force additional handshakes and HELLO negotiation.
- Weida has no application session state, usable TLS ticket reuse across fresh dials, 0-RTT attach, or exposed/tested migration contract, and none of that changes with 0031. Today it also has no automatic reconnect: `PeerSet::pick` reports `ConnectionLost(cause)` and the application dials again. Weida 0031 decides that a dialled address outlives its connection (runtime redial with backoff, subscriber filters re-sent, every transition on a `PeerEvent` stream) and that a redial answering with a different key is refused as `PeerChanged`. Terminal resume belongs to Zeughaus in both worlds; only the dial loop moves.
- The current workspace stores `iced::pane_grid::Pane` and `pane_grid::State` directly. Those IDs are local widget state and cannot become mux identity. Iced 0.14 can rebuild a pane grid from a recursive `Configuration`, so a stable runner-owned tree can remain independent from iced.
- The current graph view state is global in `App`; this cut must retain exactly one Graph surface. Terminal surfaces may be arbitrarily split or tabbed.
- The current QUIC listener authenticates the runner only. A remote shell requires client authentication before a non-loopback mux listener is enabled.
- Iced 0.14 supports a stateful custom widget and a persistent `iced_wgpu::primitive::Pipeline`. The terminal should submit one custom primitive per surface, not one widget/text object per cell or row.

## Decisions and invariants

### Authority and lifetime

- `zeughaus-runner` is the mux authority for its process lifetime. A GUI disconnect only detaches; it never kills a terminal.
- Explicitly closing a terminal pane kills its child and removes the leaf. Closing a tab kills all terminal children in that tab. Runner exit ends all terminal sessions.
- Terminal sessions do not migrate between runner processes. A hot-standby graph runner starts with a new runner incarnation and an empty mux workspace after takeover.
- The mux service is independent of the 50 ms graph host loop. PTY reads, parsing, delta generation, QUIC writes, and child waits run on dedicated Tokio/blocking tasks.
- The runner owns shared structure: tab order, group/color/title metadata, split tree/ratios, pane IDs, surface kind, and terminal IDs.
- Each GUI owns presentation state: active tab, focused pane, tab-bar placement, terminal viewport, selection, hovered link, IME/preedit, font/DPI, and blink clock.
- With no reachable runner, the editor retains a graph-only local workspace and graph editing remains usable. Terminal creation and shared structural mutation are disabled until mux attach succeeds. A remote workspace snapshot becomes authoritative on attach.

### Surface behavior

- `Surface::Graph` remains unique because graph camera/selection/navigation state is currently global.
- `Surface::Terminal(TerminalId)` is first class. New tabs create a terminal using a runner-configured default shell profile. Splitting a terminal pane creates another terminal; no network-supplied shell command string is executed.
- An Empty surface remains only as a transient reconstruction/failure state, not the normal result of a split.
- Terminal titles follow OSC/title state unless the shared tab or pane has an explicit user override.

### Control lease

- Any authenticated client may view a terminal.
- Exactly one client instance controls input, mouse, focus reporting, and PTY size for a terminal.
- The first focused client acquires an unowned terminal automatically. A second client is read-only and gets an explicit Take Control action. Taking control revokes the previous controller immediately and emits an ordered lease event.
- A brief reconnect grace binds the lease to `(authenticated principal, client instance id)` so a network blink does not make the terminal read-only. Explicit takeover bypasses the grace.
- Disconnect never queues or replays keyboard/paste input. Ambiguous input is discarded; terminal state is resynchronized from the runner.

### Performance model

- One process-wide Weida `Runtime`, one stable `/mux` URL, and one retained `Requester`/peer per runner.
- One long-lived bidirectional control exchange carries hello, workspace snapshots, topology commands, correlated replies, and lifecycle/lease events.
- One long-lived bidirectional exchange per attached terminal carries client input/viewport commands in the request direction and snapshots/deltas in the reply direction. QUIC directions are independent; a large output reply does not serialize input behind it.
- Scrollback range fetches use additional short exchanges on the same `/mux` connection, so they cannot head-of-line block control or live terminal streams.
- The initial workspace attach includes the recursive topology and bounded visible snapshots for terminal leaves, allowing first paint before per-terminal streams finish attaching.
- PTY bytes are never sent as the remote rendering contract. The runner parses once and sends canonical row state/damage to every client.
- Terminal output is parsed immediately. Subscriber notifications are coalesced through latest-sequence/watch state at an 8–16 ms render cadence; no unbounded delta queue is allowed.
- A slow subscriber computes its next delta from its last successfully sent sequence and the current canonical screen. It does not make the PTY/parser wait and does not receive every intermediate frame.
- Ordered side effects such as exit, bell count, lease change, and explicit command replies are not dropped into the row-coalescing bucket.
- Attach never sends full scrollback. It sends visible rows plus a bounded prefetch window; stable-row pages are fetched on demand and held in a bounded client LRU.
- Reconnect keeps the last visual cache on screen with a Reconnecting overlay, redials immediately, then backs off with jitter; once Weida B-270 lands the redial and backoff are Weida's `ReconnectPolicy` and Zeughaus reacts to `PeerEvent::Lost`/`Connected` instead of running its own loop. A successful reconnect performs full topology mark/sweep and fresh visible snapshots. No raw output replay buffer or incremental delta-history protocol is required because the runner already has canonical current screen and scrollback.
- Input serials are echoed in terminal updates. The GUI rejects cursor state older than its latest acknowledged input serial and measures input RTT. The first cut does not invent unsafe local text echo prediction.

### Security

- Persist a stable runner server identity and native client identity under an owner-only Zeughaus state directory; do not generate the remote server identity in memory on every start.
- Bind Weida with `ServerTls::new(identity).require_client(trust)` and dial with `ClientTls::new(server_trust).with_identity(client_identity)`.
- Authorize every mux exchange using Weida's proved `IncomingMeta.peer`; opaque terminal IDs are not credentials.
- Native local installs may bootstrap one owner-only credential bundle atomically. Remote machines use explicitly provisioned client material until a separate passkey enrollment flow exists.
- Refuse to enable `/mux` on a non-loopback bind unless stable server identity and client trust are configured. Existing non-terminal feeds may retain their current degradation behavior, but shell access must fail closed.
- Default shell command/profile, environment additions, working-directory policy, scrollback size, OSC 52 clipboard, downloads, hyperlinks, and terminal protocol resource limits are runner policy. The client requests a profile ID, not arbitrary argv/environment.
- OSC 52 writes and terminal-triggered file/download actions are disabled by default. Link opening requires an explicit GUI action and never executes a terminal-supplied command.
- Every frame, row count, cell count, string, paste, scrollback range, and decoded allocation is bounded before allocation. Malformed or unknown messages produce protocol errors, never panics.

## Shared model and wire protocol

Create a pure `zeughaus-mux` workspace crate used by runner and editor, including wasm builds for topology deserialization. It must contain no PTY, WezTerm, iced, Tokio, or Weida dependency.

### Stable model

- `RunnerIncarnation([u8; 16])`, generated once per runner process.
- `ClientInstanceId([u8; 16])`, generated once per GUI process and retained across redials.
- fixed-width `WorkspaceId`, `TabId`, `PaneId`, `TerminalId`, and `RequestId` newtypes.
- `WorkspaceSnapshot { incarnation, revision, tabs }`.
- `TabSnapshot { id, title, group, accent_rgba, root }`.
- `PaneNode::{Split { axis, ratio, first, second }, Leaf { pane_id, surface }}`.
- `SurfaceRef::{Graph, Empty, Terminal(TerminalId)}`.
- `TerminalHead` carrying epoch, current sequence, dimensions, visible stable-row range, cursor, title, modes/palette needed for rendering, bounded row data, exit state, and controller identity summary.

### Framing

- A fixed frame header carries bounded payload length, protocol major/minor, stable numeric message kind, flags, and request ID. Payloads use `postcard`/serde DTOs; enum ordering is not the wire discriminant.
- Reject a declared length over the negotiated per-kind maximum before allocating.
- Negotiate capabilities in `ClientHello`/`ServerHello`; a major mismatch rejects attach, while unknown optional kinds/capabilities can be ignored explicitly.
- Compression is negotiated. Only large snapshots/range replies above a measured threshold may use zstd level 1; live deltas and input remain uncompressed. Keep compressed and expanded size caps.
- Golden-byte tests lock the frame header and stable message-kind assignments. Behavioral round-trip tests cover snapshot/delta/input bodies and all bounds.

### Terminal rows

- `RowData { stable_row, row_seq, wrapped, spans }`.
- `CellSpan { start_col, cell_count, text, style }` coalesces adjacent cells with equal style. `cell_count` preserves terminal width independently from UTF-8 length and shaping.
- `CellStyle` contains wire-stable foreground/background/underline colors and terminal flags; never serialize `wezterm_cell::CellAttributes`.
- `TerminalDelta { terminal, epoch, from_seq, to_seq, input_serial_ack, dimensions?, cursor?, title?, modes?, evicted_before?, row_replacements, ordered_events }`.
- The receiver accepts a delta only when incarnation/epoch match and `from_seq == applied_seq`. Any gap triggers a fresh pane snapshot; it is never patched speculatively.
- Cursor state is independent from row damage. Old and new cursor overlays are invalidated without reshaping their rows.
- Image placements/resources have reserved message kinds and row references but are not produced in this cut; unsupported terminal graphics render a bounded placeholder and cannot make the GUI open local paths or URLs.

## Implementation phases

### Phase 1: Dependency and protocol boundary

1. Add `zeughaus-mux` and `zeughaus-terminal` to the root workspace.
2. Pin `wezterm-term` to the researched WezTerm commit `2658f629cd7251ce63a1698f238da2585676aa4e` behind `zeughaus-terminal`; use published `portable-pty` 0.9.0. Do not enable WezTerm serde for the wire protocol.
3. Add `postcard`, bounded frame helpers, and optional zstd only to `zeughaus-mux`.
4. Implement the stable IDs, recursive workspace model, terminal DTOs, fixed frame header, per-kind caps, version negotiation, and codec errors.
5. Add golden framing tests, malformed/oversized input tests, and delta precondition tests.
6. Record dependency licenses through the repository's existing dependency audit. Reimplement mux/cache algorithms from the observed behavior rather than copying WezTerm source.

### Phase 2: Native terminal engine

1. In `zeughaus-terminal`, define a small `TerminalConfiguration` implementation with bounded scrollback and runner policy; keep WezTerm types private to the crate.
2. Implement `TerminalSession::spawn(profile, size)` around `native_pty_system().openpty`, `CommandBuilder`, child handle/killer, master reader/writer, and `wezterm_term::Terminal`.
3. Use a blocking PTY reader to deliver bounded byte chunks to a per-session actor. The actor is the sole terminal-model mutator; it handles output, semantic key/text/paste/mouse/focus commands, resize, and child exit serially.
4. Convert iced-neutral key DTOs to WezTerm/termwiz input at the runner, where active keyboard/application modes are known. Never encode ANSI key sequences in the GUI.
5. Use WezTerm stable-row/sequence APIs to atomically build visible snapshots, changed-row ranges, row replacements, cursor/title/mode changes, and scrollback pages under the session lock/actor turn.
6. Publish latest-sequence notifications through bounded `watch` state. Preserve final screen and exit status after child exit until the pane is explicitly closed.
7. Add engine tests for ANSI styles, wide/combining Unicode, alternate-screen enter/leave, resize/rewrap, changed-row deltas, bounded scrollback eviction, semantic key input, child exit, and kill-on-explicit-close.

### Phase 3: Runner mux service and workspace authority

1. Add `zeughaus-runner/src/mux.rs` as a service parallel to graph execution. It owns `WorkspaceActor`, `SessionRegistry`, authenticated clients, request deduplication, controller leases, and subscriber tasks.
2. Register exactly one `MUX_PATH = "/mux"` replier/acceptor on the existing listener. For each accepted exchange, read and validate the first operation frame, call `IncomingRequest::take_body()` before `reply()`, then run request and reply directions concurrently.
3. Implement operation kinds for control attach, terminal attach, row fetch, and reserved resource fetch. Keep every operation on `/mux` so Weida reuses one QUIC connection.
4. Serialize all topology mutation in `WorkspaceActor`. Broadcast coalesced full topology snapshots/revisions rather than a fragile collection of split/tab patch variants; terminal row traffic remains per-pane.
5. Implement correlated, idempotently deduplicated topology RPCs: new terminal tab, split with terminal, close pane/tab, resize split, rename/group/color, and take/release control.
6. Implement the one-Graph-surface invariant and server-side default workspace. New terminal creation resolves only runner-configured profile IDs.
7. Attach returns hello, authoritative topology, and bounded current terminal heads in one response. Terminal stream attach starts from that head sequence or returns a fresh pane snapshot if it changed.
8. Implement per-subscriber delta generation, watched stable-row range, scrollback fetch, slow-client limits, cancellation cleanup, input serial acknowledgements, reconnect lease grace, and explicit takeover.
9. Keep mux sessions alive across GUI detach and graph-store reconnects; runner process exit is the terminal lifetime boundary. Do not place terminal screen contents in SpacetimeDB.
10. Replace ephemeral transport identity startup with credential loading/bootstrap and Weida mTLS. Gate `/mux` on authenticated client trust, especially for non-loopback binds.
11. Add a loopback integration test using real Weida exchanges: create shell, receive snapshot, send input, observe delta, resize, attach a viewer, reject viewer input, transfer control, drop both streams, keep child running, reconnect, and receive current screen.

### Phase 4: Editor transport and remote cache

1. Extract the process-wide native Weida runtime, endpoint derivation, client TLS, and reconnect policy from `zeughaus/src/feed.rs` into a shared native `transport` module; frames/events and mux must use the same runtime without duplicating connection state.
2. Add `zeughaus/src/mux.rs` with one retained requester/control task per runner endpoint. Start it from runtime reconciliation, never from `view()`.
3. On endpoint change or connection loss, increment a connection epoch, keep last visual state, discard pending input, fail/resolve ambiguous requests safely, redial immediately, then use jittered capped backoff. Keep the redial behind one small interface so that, when Weida B-270 is available, the loop is deleted and the same epoch bump is driven by `PeerEvent::Lost` and `PeerEvent::Connected`. Note the 0031 §4.7 consequence: a transparent Weida redial only reaches a runner whose server identity is stable across restarts, which is what Phase 3.10 provides; with the current in-memory identity every runner restart is a new peer and Weida will report `PeerChanged` rather than reconnect.
4. Send `ClientHello` and cached incarnation/topology/pane sequence hints. Reconcile every successful attach from the authoritative snapshot by stable IDs; discard caches on runner-incarnation or terminal-epoch change.
5. Add one terminal stream task per terminal surface referenced by the active workspace, plus bounded range-fetch tasks for viewport misses. Old task messages carry an epoch and are rejected after replacement.
6. Implement `TerminalViewState` with bounded stable-row LRU, applied sequence, dimensions, cursor, title/modes, dirty rows, viewport/prefetch range, status, controller state, input serial/RTT, selection, IME, and render revision.
7. Apply snapshots atomically. Apply deltas only on exact epoch/base sequence. Range replies include actual first retained row and request generation so an older fetch cannot overwrite a newer row update.
8. Add editor state/message integration for mux status, snapshots/deltas, input, paste, mouse, resize, viewport, control takeover, process exit, and topology command results.

### Phase 5: Stable workspace reconstruction

1. Refactor `zeughaus/src/workspace.rs` to separate stable `zeughaus_mux::PaneNode`/IDs from ephemeral iced `Pane`/`Split` IDs.
2. Rebuild `pane_grid::State` with `State::with_configuration`, retaining maps between stable `PaneId` and iced panes. Preserve active/focused stable IDs best-effort after every snapshot.
3. Split workspace messages into local presentation actions and remote structural commands. Active tab/pane and tab placement stay local; tab/split/surface changes go through the mux control task.
4. Make New Tab create a default terminal, and H/V split create a terminal leaf. Keep one graph leaf and render a clear unavailable/reconnecting surface when its runner is absent.
5. Coalesce split ratio updates while dragging and send only the newest revision; accept the authoritative returned topology.
6. Closing a pane/tab sends an explicit destructive mux RPC. Merely closing the editor or losing the network sends no close operation.
7. On wasm, deserialize the same topology but render terminal surfaces as non-interactive placeholders; no PTY or native Weida dependency enters the wasm graph.

### Phase 6: Production iced terminal widget and GPU pipeline

1. Add `zeughaus/src/terminal/` with a single stateful `TerminalWidget` per surface, following iced's `Widget`/`Tree` lifecycle and submitting one clipped `TerminalPrimitive`.
2. Implement a shared `TerminalPipeline` through `iced_wgpu::primitive::Primitive` with persistent per-view row arenas, reusable staging buffers, glyph atlas, background/glyph/decoration instance buffers, and a fixed small draw sequence.
3. Use iced-compatible `cosmic-text`/fontdb/swash shaping and rasterization rather than depending on WezTerm's unpublished, config-heavy `wezterm-font` crate. Copy WezTerm's cache decomposition: row shape cache separate from complete row instance cache.
4. Key shaped rows by row content/style hash plus font/metrics/bidi generations. Key glyph atlas entries by face/style, glyph ID, pixel size, scale factor, subpixel bucket, and color presentation. A changed row replaces only its arena ranges; cursor-only changes update only cursor instances.
5. Layer base/backgrounds, selection/cell backgrounds, glyphs, decorations/cursor in deterministic order. Reserve under-text and over-text image layers in the primitive layout without implementing image transfer in this cut.
6. Compute terminal columns/rows from content bounds and measured cell metrics. Send resize only when integer cell geometry changes, and only from the controller; debounce drag storms.
7. Handle focus, semantic keys, committed text/IME, bracketed paste, mouse reporting, local selection, copy, scrollback, hyperlink hover/activation, and Take Control. `Ctrl+Shift+C/V` copy/paste; `Ctrl+C` remains terminal input. A dedicated release-focus shortcut prevents global application shortcuts and shell input from firing together.
8. Keep blink and animation GUI-local. Request redraw only at the exact next cursor/text blink deadline; idle terminals must not run a permanent 30/60 Hz subscription.
9. Add deterministic CPU-side tests for row hashing/cache invalidation, dirty-row arena replacement, cursor-only invalidation, geometry-to-cell resize, selection extraction, and input routing. Validate GPU behavior through the actual application surface rather than source-text tests.

### Phase 7: End-to-end proof, performance gates, and cleanup

1. Run a real local runner/editor and exercise shell startup, typed input, paste, colors/styles, Unicode wide/combining text, alternate screen (`vim` or equivalent), mouse mode, resize/rewrap, deep scrollback, split/tab creation, explicit close, and process exit.
2. Open a second editor against the same runner. Verify both render live state, only the controller can type/resize, Take Control revokes the first client, closing either GUI preserves the child, and reconnect shows the current screen without replaying input.
3. Force a Weida connection drop while the child continues producing output. Verify last-known rendering plus Reconnecting state, immediate redial, full topology reconciliation, current visible snapshot, lazy scrollback recovery, and rejection of stale old-epoch messages.
4. Verify security with missing, untrusted, and trusted client identities. Non-loopback mux startup must fail closed without client trust; existing authenticated sample/event behavior must remain functional.
5. Add release-mode benchmarks/instrumentation for parser+damage, codec size/time, warm stream attach, full fresh redial, high-rate full-screen output, one-cell updates, and dirty-row GPU uploads. Record p50/p95 and allocation/byte counts.
6. Performance acceptance:
   - warm attach uses the already pooled connection and no TLS/QUIC/HELLO handshake;
   - first paint requires one control exchange response, not serial per-pane round trips;
   - fresh reconnect requires only current Weida QUIC/TLS/HELLO plus one application attach before first paint;
   - loopback warm first-paint p95 target is 10 ms and fresh-redial p95 target is 50 ms in release builds on the project workstation;
   - delta encode/network/CPU cost scales with changed watched rows, not total scrollback or total terminal count;
   - a cursor-only update performs no row shaping;
   - an idle non-blinking terminal schedules no redraws;
   - subscriber queues, row caches, frame sizes, scrollback, and GPU atlases remain bounded.
7. Run `cargo fmt --check`, native workspace `cargo check`, wasm editor check, workspace tests, and `cargo clippy --workspace --all-targets -- -D warnings` serially.
8. After the runtime smoke and performance checks pass, remove throwaway harnesses, retain only contract-defending tests/benchmarks, and update `DESIGN.md` and `CLAUDE.md` with mux authority, one-path QUIC topology, security provisioning, lifecycle, and measured connect/reconnect behavior.

## Affected files

### New

- `zeughaus-mux/Cargo.toml`
- `zeughaus-mux/src/lib.rs`
- `zeughaus-mux/src/codec.rs`
- `zeughaus-mux/src/workspace.rs`
- `zeughaus-mux/src/terminal.rs`
- `zeughaus-terminal/Cargo.toml`
- `zeughaus-terminal/src/lib.rs`
- `zeughaus-terminal/src/session.rs`
- `zeughaus-terminal/src/screen.rs`
- `zeughaus-runner/src/mux.rs`
- `zeughaus/src/mux.rs`
- `zeughaus/src/transport.rs`
- `zeughaus/src/terminal/mod.rs`
- `zeughaus/src/terminal/widget.rs`
- `zeughaus/src/terminal/pipeline.rs`
- `zeughaus/src/terminal/terminal.wgsl`

### Modified

- `Cargo.toml`: workspace members/dependencies and pinned terminal dependencies.
- `zeughaus-runner/Cargo.toml`: mux/terminal, codec, TLS/config dependencies.
- `zeughaus-runner/src/main.rs`: mux service startup, listener registration, credential policy, lifetime.
- `zeughaus-runner/src/transport.rs`: stable identity, `ServerTls`, client trust, mux-safe startup.
- `zeughaus-runner/src/runner.rs`: expose owner/runtime discovery state without placing terminal I/O in the graph executor.
- `zeughaus/Cargo.toml`: native mux client and custom renderer dependencies; wasm remains PTY-free.
- `zeughaus/src/main.rs`: native mux/terminal/transport modules and credential options.
- `zeughaus/src/feed.rs`: reuse extracted shared Weida runtime/TLS/retry plumbing.
- `zeughaus/src/message.rs`: mux/workspace/terminal events and input commands.
- `zeughaus/src/app.rs`: mux/client state, runtime reconciliation, message routing, workspace rendering, shortcut/focus precedence.
- `zeughaus/src/workspace.rs`: stable topology projection, terminal surfaces, remote structural commands.
- `DESIGN.md`: terminal/mux ownership, wire/QUIC topology, rendering, security, reconnect.
- `CLAUDE.md`: updated architecture and run/provisioning instructions after proof.

## Explicit scope boundaries

- No full WezTerm mux/client/server/codec/GUI dependency.
- No SSH tunnel or SSH-based credential bootstrap.
- No Weida protocol change, 0-RTT claim, TLS-resumption claim, or migration guarantee. Weida 0031's redial is used when available and not depended on for correctness: the mux resync must be correct against a fresh application-level connection either way.
- No PTY/process state in the GUI and no terminal contents in SpacetimeDB.
- No simultaneous multi-writer terminal input.
- No queued input replay after an ambiguous disconnect.
- No multiple independent Graph surfaces in this cut.
- No terminal image resource transfer in the selected first production cut; only stable protocol/renderer extension seams and safe placeholders.
