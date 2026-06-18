# SpacetimeDB-Integration + wasm32-Target

## Context

SpacetimeDB ist im Code noch nicht eingebunden — es steht nur als Plan in `DESIGN.md`/`CLAUDE.md` und war bewusst zurueckgestellt, bis der Rust-SDK WASM-Support landet (PR #4183). Dieser Support ist jetzt da: `spacetimedb-sdk` 2.6.0 hat ein `browser`-Feature (gloo-net, wasm-bindgen, web-sys; `tokio`/`native-tls` auf non-wasm32 gegated). Damit faellt der einzige genannte Blocker weg.

Ziel laut Nutzer: SpacetimeDB als **zentraler Store** mit **kollaborativem Editieren**, und das **wasm32-Target jetzt mit aufsetzen**. Voller Sync (lokalen autosave ersetzen) darf spaeter kommen, wenn der Plan so weit ist. Dieser Plan legt daher das Fundament: Store-Schema (Modul), Client-Anbindung, und wasm32-Buildfaehigkeit — ohne den bestehenden lokalen autosave schon zu entfernen.

## Scope: jetzt vs. spaeter

**Jetzt (dieser Plan):**
- Phase 1: SpacetimeDB-Modul-Crate mit Graph-Schema (zentraler Store).
- Phase 2: Client-SDK 2.6 als Dependency + generierte Bindings + Verbindungsmodul (Connect/Subscribe). autosave bleibt vorerst Primaerquelle.
- Phase 3: wasm32-Target aufsetzen — native-only Code cfg-gaten, per-Target-Deps, Build-Tooling. Ziel: `cargo build --target wasm32-unknown-unknown` gruen + dokumentierter Browser-Run-Pfad.

**Spaeter (eigene Folge-Iteration, hier nur skizziert):**
- Phase 4: Voller Sync — Reducer-Aufrufe aus `update()`, Live-Tabellen-Callbacks ins Editor-State, autosave durch SpacetimeDB ersetzen, serverseitige ID-Vergabe fuer Kollaboration.

## Prerequisites (vom Nutzer auszufuehren / zu bestaetigen)

- `spacetime` CLI ist v2.1.0, SDK-Ziel ist 2.6.0. Fuer `spacetime generate` und einen lokalen Host mit passendem Wire-Format auf 2.6 anheben: `spacetime version upgrade` (Installations-Aktion — ich stosse sie nicht ungefragt an).
- Fuer den Browser-Build wird `trunk` benoetigt (kein `wasm-bindgen`/`trunk` installiert; `wasm-pack` vorhanden): `cargo install trunk wasm-bindgen-cli`.

## Phase 1 — SpacetimeDB-Modul (zentraler Store)

Neues Crate `zeughaus-module/`, **aus dem Workspace ausgeschlossen** (`[workspace] exclude = ["zeughaus-module"]` in der Root-`Cargo.toml`), damit natives `cargo build`/`cargo check` es nicht mitbaut. Es wird via `spacetime build` (Target wasm32) gebaut.

- `zeughaus-module/Cargo.toml`: `crate-type = ["cdylib"]`, dep `spacetimedb = "2.6"`.
- `zeughaus-module/src/lib.rs`: Tabellen spiegeln `GraphDocument` (zeughaus-core/document.rs):
  - `node`: `id u64 (#[primary_key])`, `type_id String`, `display_name String`, `x f32`, `y f32`, `params String` (JSON-kodierte Param-Liste — vermeidet verschachtelte Tabellen im ersten Wurf).
  - `edge`: `id u64 (#[primary_key])`, `from_node u64`, `from_pin String`, `to_node u64`, `to_pin String`.
  - (Kollaborations-Scoping wie `graph_id`/Identity bewusst erst in Phase 4.)
  - Reducer: `create_node`, `move_node`, `set_node_params`, `delete_node`, `connect_edge`, `disconnect_edge`, plus `replace_graph` (Bulk, fuer den heutigen Load-Flow).
- Build-Verifikation: `spacetime build` im Crate.

## Phase 2 — Client-SDK-Fundament

- Root-`Cargo.toml`: `spacetimedb-sdk = "2.6"` in `[workspace.dependencies]`.
- Bindings generieren: `spacetime generate --lang rust --out-dir zeughaus/src/sync/bindings --project-path zeughaus-module` (eingecheckter generierter Code).
- Neues Modul `zeughaus/src/sync/mod.rs`: `DbConnection`-Aufbau (connect, subscribe auf `node`/`edge`), hinter einer kleinen Fassade. **Noch keine** Verdrahtung in `update()` — autosave bleibt Primaerquelle. Ziel dieser Phase: kompiliert, verbindet, subscribed (Smoke-Test gegen lokalen Host).
- `zeughaus/Cargo.toml`: `spacetimedb-sdk.workspace = true` als Dependency.

## Phase 3 — wasm32-Target aufsetzen

Kernarbeit: native-only Code im Binary konditional kompilieren und per-Target-Dependencies einfuehren.

- `zeughaus/Cargo.toml` umbauen:
  - `iced` Basis-Features `["advanced","wgpu"]`; `tokio`-Feature nur nativ.
  - `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`: `zeughaus-process`, `zeughaus-capture`, `rfd`, `iced` (mit `tokio`).
  - `[target.'cfg(target_arch = "wasm32")'.dependencies]`: `iced` (ohne tokio), `web-time`, ggf. `gloo-storage`, `console_error_panic_hook`, `wasm-bindgen`.
- `app.rs` cfg-gaten (alle Fundstellen aus der Exploration):
  - Plugin-Registrierung (app.rs:75-80): `ProcessPlugin`/`CapturePlugin` nur nativ; wasm registriert nur `TransformPlugin`.
  - Persistenz: `autosave()`/`load_autosave()`/`autosave_path()` (app.rs:250-272) nativ; wasm = no-op-Stub (Persistenz kommt mit SpacetimeDB in Phase 4).
  - `rfd`-Dialoge `SaveGraph`/`LoadGraph` (app.rs:579/596) nur nativ.
  - `std::time::Instant` (app.rs:236) → `web_time::Instant` (cross-platform).
  - `std::env::current_exe` (app.rs:258) nur nativ.
- `zeughaus/index.html` + `Trunk.toml` fuer den Browser-Build; `main.rs` ggf. `console_error_panic_hook` im wasm-Pfad.
- Verifikation: `cargo build --target wasm32-unknown-unknown -p zeughaus` gruen. Live-Browser-Run (`trunk serve`) als dokumentierter Pfad + Verifikations-Milestone — siehe Risiken.

## Deferred — Phase 4 (voller Sync, spaetere Iteration)

- Reducer-Aufrufe aus den Mutationspunkten in `update()` (EdgeConnected/Disconnected, GroupMoved, DeleteNodes, SpawnNode, ConstValueChanged).
- Tabellen-Callbacks (`on_insert`/`on_update`/`on_delete`) → Editor-State + Re-Execute.
- autosave durch SpacetimeDB als Quelle der Wahrheit ersetzen.
- **Serverseitige ID-Vergabe**: heutige globale Atomic-Counter (`NodeId::next()`/`EdgeId::next()`, id.rs:15) kollidieren ueber Clients — Modul vergibt IDs via `#[auto_inc]`.

## Risiken

- **iced auf wasm + WebGPU**: `iced_nodegraph` ist explizit "WebGPU only, kein WebGL-Fallback". Der Browser-Editor laeuft nur mit WebGPU-faehigem Browser. Ob iced 0.14 + iced_nodegraph 0.1.0 sauber fuer wasm32 bauen und rendern, ist das groesste Restrisiko und erst beim Build sicher. Plan-Ziel ist daher primaer "kompiliert fuer wasm32 + Tooling steht"; Live-Render-Hardening kann eine Folgeaufgabe werden.
- **SDK-Version vs. Host**: lokaler Host muss 2.6 sein, sonst Wire-Format-Mismatch bei `generate`/connect.

## Verification

- Phase 1: `spacetime build` im Modul-Crate erfolgreich.
- Phase 2: nativer `cargo check`/`clippy -D warnings`/`test` weiterhin gruen; Connect/Subscribe gegen lokalen `spacetime start` (manueller Smoke-Test).
- Phase 3: `cargo build --target wasm32-unknown-unknown -p zeughaus` gruen; nativer Build unveraendert gruen; `trunk serve` startet (Live-Render = best effort/Milestone).
- Gesamt nativ: `cargo check`, `cargo test`, `cargo clippy -- -D warnings` (Pre-Push-Checkliste).
