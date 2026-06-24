# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Companion Project

The iOS client lives at `~/Documents/Audio Share/` (Swift/SwiftUI). This repo is the server. The two must stay in sync on the wire protocol and QR payload format described at the bottom of this file.

## Product Vision & Roadmap

**What Audio Share is:** open, self-hostable software that turns any ordinary (non-smart) speaker or amp into a networked, multi-room audio endpoint. It runs on a Raspberry Pi (or similar Linux device) that the user flashes/installs themselves. A user picks audio on their phone and plays it to any speaker on the network. This Rust service is the device that actually drives the speaker.

**Distribution & goals (pivot, 2026-06):** Audio Share is being built as (1) **downloadable software** a user installs on their own hardware — the Volumio / moOde / Home Assistant model, *not* a hardware product we manufacture — and (2) a **portfolio/showcase project**. These goals reinforce each other: open-source core, a build that actually runs and demos end-to-end, polished setup UX, clean docs. There is no FCC/CE/inventory/hardware-margin path; the value is the software and the experience.

**Strategic pivot — from streaming aggregator to open endpoint/receiver.** The original vision (the device fetches and streams Spotify/Apple Music/Pandora/YouTube *server-side*) is **not viable** for an indie/self-hosted product: those services forbid raw-audio capture, DRM (Widevine/EME) only decrypts at the player in real time, and Spotify's device program (eSDK) is approved-organizations-only and contractually forbids combining its content with other services. So we **stop trying to *be* a streaming service** and instead **become an endpoint that other apps play *to***, plus a player for open/DRM-free sources. This sidesteps licensing entirely — legally we are a speaker.

**Two ways audio reaches a speaker:**
- **Sources the device plays itself (DRM-free, we hold the encoded bytes):** internet radio, podcasts (RSS), self-hosted libraries (Subsonic/Navidrome, Jellyfin, Plex), and local/phone-relayed files. These are the legal core and ship first.
- **Receiver protocols (the phone's existing app streams to us):** AirPlay 2 (via `shairport-sync`), optionally Spotify Connect (via `librespot`) and Chromecast. The user authenticates Spotify/Apple Music on their *own* phone and pushes to the device — no licensing exposure for us. Gray-area integrations like `librespot` ship as **optional, user-installed plugins**, never bundled, so responsibility sits with the user (the Volumio/moOde model).

**Core differentiator — multi-room:** send *different* audio to *different* speakers at once (independent per-zone playback: an output registry, zone grouping, independent routing per output), and group outputs for *synchronized* playback of the same source. We **adopt Snapcast for synchronized multi-room** rather than building clock sync from scratch. Note that Sonos/WiiM already do both; our real differentiation is setup simplicity and being open/self-hostable.

**Per-source play-mode** still organizes the engine: `server-side fetch+buffer` (we hold encoded bytes), `relay-from-phone` (phone sends local-audio bytes), `receiver` (an external protocol delivers audio to us), and a last-resort `loopback capture` (single-zone, ToS-gray, optional) for API-less platforms.

**Current state (as of 2026-06):** networking + security foundation is solid — TCP server, the two-phase X25519/HKDF/AES-GCM handshake, sessions, mDNS. **Phase 1 (first end-to-end audio path) is done:** a `play` task with a stream URL drives a real engine (`audio/engine.rs` + `audio/decode.rs`) that fetches a DRM-free HTTP internet-radio stream, decodes (Symphonia, mp3/aac), resamples (Rubato), and plays it through the `audio/output.rs` cpal sink behind the `audio/sink.rs` `AudioSink` boundary; `stop` halts it. **Phase 2 scaffolding is in:** an `OutputRegistry` (`audio/registry.rs`) and per-zone routing in the engine, plus an optional `zone` in the `play`/`stop` protocol — currently shipping with one `"default"` zone → the local output, so behavior matches single-zone. **Change 5 sub-step 1 building blocks are in (`audio/snapcast.rs`):** a `SnapcastSink` (`impl AudioSink`, converts decode's `f32` PCM → `s16le` into a `snapserver` input FIFO) and a thin `SnapserverSupervisor` (spawns/restarts a `snapserver` with one pipe stream). **Change 5 sub-step 2 is underway:** the project is now a **Cargo workspace** with a shared `crates/protocol` (`audioshare_protocol`) crate holding the hub↔dongle control wire types (2.1), and the hub has a **dongle registration listener** (`server/dongle_server.rs`, port 50506) wired to `Engine::register_dongle`/`dongle_offline`, which spawns/supervises `snapserver` and registers dongles as Snapcast outputs (2.2). **Change 5 sub-step 2.3 is in:** the custom **dongle agent** (`crates/dongle_agent`, a workspace binary sharing `audioshare_protocol`) — persisted identity (UUID + name), app-mediated assignment (mDNS advertise as `_audioshare-dongle._tcp` + an assignment listener on port 50507 that takes `AppToDongle::Assign`), hub registration (`DongleToHub::Register` → `HubToDongle::Registered`, then holds the connection as a liveness signal), and a `SnapclientSupervisor` that spawns/restarts `snapclient` pointed at the hub's `snapserver` (the agent *delegates* transport/clock-sync to Snapcast — it never reimplements them). Hub resolution order: `--hub` dev flag > persisted assignment > wait for app assignment. **Change 5 sub-step 2.4 is in:** the dongle↔hub session is now resilient — **exponential reconnect backoff** (agent `next_backoff`: `BACKOFF_BASE` 1s, doubling to `BACKOFF_MAX` 30s, resetting after a `BACKOFF_RESET_AFTER` ≥30s healthy session — longer than the heartbeat timeout so a heartbeat-killed session doesn't reset it) and a **heartbeat**: the agent sends `DongleToHub::Heartbeat` every `HEARTBEAT_INTERVAL_SECS` (5s) on a dedicated task (so the timed read is never cancelled mid-line — `read_line` isn't cancel-safe), the hub replies `HubToDongle::Heartbeat`, and both ends read with a `HEARTBEAT_TIMEOUT_SECS` (15s) timeout so a WiFi dropout that never delivers a TCP FIN is caught within that window (hub marks the output offline; agent reconnects) rather than hanging on a half-open socket. Heartbeat consts/variants live in `crates/protocol`. The hub's `DongleServer` reaches the engine through a `DongleRegistrar` trait (production `EngineRegistrar` → `ENGINE`) so connection handling is covered by **device-free loopback tests**: register→heartbeat-reply→offline against a mock registrar (`server/dongle_server.rs`), the agent's `Assign`→`Assigned` handshake (`crates/dongle_agent/src/assignment.rs`), and protocol heartbeat round-trip; the `snapclient`/`snapserver` audio path stays demo-gated, not in CI. **Proven end-to-end on 2026-06-20** (Pi hub → laptop agent + stock `snapclient` → audio). Setup gotcha: **disable the distro `snapserver.service`** (`sudo systemctl disable --now snapserver`) — it auto-starts a `snapserver` that squats on 1704/1705 and blocks the hub's own supervised one (full detail in `docs/multi-room-plan.md` "Bring-up notes"). **Change 5 sub-step 2.5 is in (iOS side, cross-project in `~/Documents/Audio Share/`):** scan-and-assign — the iOS app browses `_audioshare-dongle._tcp` (`Managers/DongleDiscoveryManager.swift`, parallel to the hub-discovery `ServiceDiscoveryManager`), lists discovered dongles (`Controllers/Library/DongleListView.swift`, reached from `Library` via an "Add a Speaker" `NavigationLink`), and on "Add to this hub" runs the assignment handshake (`Managers/DongleAssignmentManager.swift`): connect to the dongle's listener on `DONGLE_ASSIGNMENT_PORT` (50507), send newline-delimited `AppToDongle::Assign { hub_host = the paired hub's IP, hub_port = HUB_REGISTRATION_PORT 50506 }`, await `DongleToApp::Assigned { dongle_id }`. These wire types are hand-mirrored from the `audioshare_protocol` crate (the shared-crate discipline doesn't reach the iOS client). Untested on-device (dev box has Command Line Tools only, no `xcodebuild`). **Change 5 sub-step 3 is in:** per-zone Snapcast streams + grouping via a `SnapcastRouter` (`audio/snapcast_router.rs`) and a `SnapcastControl` JSON-RPC client (`audio/snapcast_control.rs`, persistent TCP to `snapserver` on `127.0.0.1:1705`). The router owns a **stream pool** of `STREAM_POOL_SIZE = 16` slots (FIFOs `/tmp/audioshare-snapfifo-{k}`, stream ids `as-{k}` for k in 0..16); `sink_for_zone(zone, dongle_ids)` allocates a free slot and returns its `SnapcastSink` to the decode thread, `release_zone` frees it. A **desired-state reconciler** converges `snapserver`'s groups/streams (`Group.SetClients` + `Group.SetStream` RPCs) on every zone change, playback change, or `Client.OnConnect`/`OnDisconnect` notification — eliminating the previous manual `Group.SetStream` workaround. Dongle `Output`s now carry `sink: None` (`Output.sink` is `Option<Arc<dyn AudioSink>>`); the router allocates a per-zone FIFO sink at play time. Zone membership is enforced all-dongle or all-local (never mixed — `mixed_zone_unsupported`). Zone CRUD methods (`create_zone`, `delete_zone`, `rename_zone`, `set_zone_outputs`) are live in the engine and exposed over the wire. Not yet built: dongle-channel auth (registration + assignment channels still unauthenticated); iOS grouping UI (own spec, wire contract frozen in this file). Not yet built from sub-step 2: proper buffering (KAN-23 general local-output path — the `SnapcastSink` backpressure was fixed in sub-step 3.1 by switching to blocking writes after FIFO open). Dead code exists (`rest_server/`, `mdb.rs`, `authentication.rs`, `user.rs`). The Go service currently does accounts + Spotify OAuth — under the pivot, server-side Spotify OAuth is no longer needed and the account model itself may become optional (pairing is the security boundary); revisit before investing further there. These gaps are tracked in Jira project **KAN**, which will be reconciled with the pivot later. **AirPlay Slice 2 is in:** per-zone classic `shairport-sync` receivers reconciled against the zone set, audio-FIFO-bracketed sessions routed through `zone_sink()` (local or dongle), `sources` push + `list_sources`; track metadata/art (Slice 3) and reroute (Slice 4) are not yet built.

### Build plan (ordered)

Sequenced as vertical slices — each phase ends at something demoable. Jira (project **KAN**) will be reconciled with this plan later; the phase numbers below are not ticket IDs.

1. **First end-to-end audio path.** Wire `commands::dispatch()` into a real playback engine; finish the `AudioOutput` drain; add a decode step; play **internet radio** (HTTP stream → decode → cpal). Goal: phone says "play `<url>`", speaker makes sound. Proves the whole pipeline and is the gate for everything else (and for any demo video).
2. **Independent multi-room.** Per-output/zone registry + routing so each output plays its own independent stream. This is the headline feature.
3. **Synchronized multi-room via Snapcast.** Integrate/supervise Snapcast for grouped, time-aligned playback instead of a hand-rolled clock.
4. **Receiver protocols.** AirPlay 2 receive (`shairport-sync`) so any iPhone app can push audio with zero licensing exposure; Spotify Connect (`librespot`) as an optional plugin.
5. **More DRM-free sources.** Podcasts (RSS), Subsonic/Jellyfin client, phone-relayed local files.
6. **Product & portfolio polish.** Flashable image / one-command installer, zero-config onboarding, jitter/underrun buffering, reconnect resilience, tests; open-source hygiene (README, architecture diagram, demo video, license). Optional monetization: pre-flashed images / pro tier / donations.

## Two Services

Historically this project produced two binaries. As of the 2026-06 pivot the Rust device server is the only one needed to run:

- **`audioshare_device`** — Rust: TCP server (port 50505) + mDNS broadcast + dongle registration (50506) + an **account-auth stub on 8080** (`server/auth_server.rs`). This is the primary service the iOS client connects to.
- **`audioshare_site`** — Go: HTTP REST API (port 8080) backed by PostgreSQL; user accounts + Spotify OAuth. **No longer required.** The iOS app gates its device screen behind a login (`/authenticateUser`, `/createUser`); the Rust auth stub now answers those with `{"success": true}` so a second binary + PostgreSQL isn't needed. Spotify OAuth is dead under the pivot, and pairing — not accounts — is the security boundary. The Go service remains in `site/` for reference but isn't deployed.

`run.sh` still references both binaries; for current testing only `audioshare_device` is run.

## Build & Run

```bash
# Rust device server (development)
cargo build
cargo run

# Rust — release build and copy to Raspberry Pi (host alias "pi")
./compile.sh

# Go site server
cd site && go build -o audioshare_site && ./audioshare_site

# Deploy both to Pi (builds and scps)
./to_pi.sh           # both
./to_pi.sh device    # Rust only
./to_pi.sh site      # Go only

# Run on Pi (expects pre-built binaries present)
./run.sh             # both, backgrounded with logging
./run.sh -v          # both, foreground verbose
./run.sh device      # Rust only

# Database migrations (Go/PostgreSQL side)
python3 migrations/migrate.py
```

**macOS caveat:** The Rust binary reads `/proc/cpuinfo` for the Pi serial number and will exit with a fatal error on macOS. Development of the TCP/security logic on macOS requires stubbing `get_serial_number()` or running on Linux.

## Architecture

### Rust service (`src/`)

**Connection flow:**
1. `main.rs` creates `Arc<Server>` and calls `server.start()`
2. `Server::start()` spawns two tokio tasks: `ConnectServer` (TCP) and `Broadcast` (mDNS)
3. On each TCP connection, `ConnectServer` reads an initial JSON message containing `{ "public_key": "<base64>" }`
4. `Security::new()` is created with the client's X25519 public key and the pairing secret
5. `Connection::start_new_connection()` sends back an encrypted session key + UUID
6. `Connection::listen()` then loops, decrypting and authenticating each subsequent message

**Key modules:**
- `server/connection_server.rs` — TCP listener (port 50505), handshake initiation, serial number read
- `server/dongle_server.rs` — Change 5 sub-step 2.2 (+ heartbeat in 2.4): dongle registration listener (port 50506), parallel to `connection_server`. Reads a newline-delimited `audioshare_protocol::DongleToHub::Register`, registers the dongle (via the `DongleRegistrar` trait — production `EngineRegistrar` forwards to `ENGINE`, so the handler is mockable in device-free loopback tests), replies `HubToDongle::Registered { snapserver_host, port }`, then holds the connection as a liveness signal: it reads with a `HEARTBEAT_TIMEOUT_SECS` timeout, replies `HubToDongle::Heartbeat` to each `DongleToHub::Heartbeat`, and on EOF/error/timeout marks the dongle offline. Control only (no audio; Snapcast carries that), **unauthenticated** for now (LAN, user-flashed dongles)
- `server/broadcast.rs` — mDNS registration as `_audioshare._tcp.local.` on port 50505
- `server/auth_server.rs` — account-auth stub (port 8080, warp). Replaces the Go `audioshare_site` for login: serves `/authenticateUser` and `/createUser`, always returning `{"success": true}` (no storage, no password check). Exists only because the iOS app gates its device screen behind a login; pairing is the real security boundary. The app POSTs to a hardcoded Pi IP here (login precedes mDNS discovery, so it can't use the discovered address)
- `server/connection.rs` — per-client loop; validates `session_token` UUID on every message
- `server/commands.rs` — parses the `task` field into `Task` and dispatches it; reads the optional target `zone` (default `"default"`) and drives `play`/`stop`/zone-CRUD on the engine; `pause`/`seek`/`volume` are still `not_yet_implemented` stubs
- `audio/engine.rs` — `Engine` + global `ENGINE`: owns the `OutputRegistry`, a `SnapcastRouter`, and one decode thread *per zone*; `play(zone, url)`/`stop(zone)`. Opens the local cpal device lazily on first play via `ensure_local` (construction touches no hardware). `FanOut` fans a zone's stream to ≥2 local outputs (unused until a real second local output exists). Dongles register via `register_dongle(id, name)` (lazily starts the router, registers a dongle `Output` with `sink: None`, auto-creates a zone `id → [id]`) and `dongle_offline(id)`. Sub-step 3: zone routing branches all-local (existing path) vs. all-dongle (`router.sink_for_zone`); mixed local+dongle is blocked at `set_zone_outputs` time and defended in `zone_sink`. Zone CRUD: `create_zone`, `delete_zone`, `rename_zone`, `set_zone_outputs`, `list_zones` (returns `Vec<ZoneView { zone, name, outputs, playing }>`). `OUTPUTS_CHANGED` broadcast fires on any topology change.
- `audio/registry.rs` — `OutputRegistry`: catalogue of `Output { id, name, sink: Option<Arc<dyn AudioSink>>, online }`; `sink(id)` resolves only *online* outputs (returns `None` for dongle outputs, which carry no sink). The local device is `id: "local"`; dongles register with `sink: None`.
- `audio/sink.rs` — `AudioSink` trait: the boundary decode writes PCM into, decoupling it from local cpal vs. a network sink.
- `audio/snapcast.rs` — `SnapcastSink` (`impl AudioSink`; `f32`→`s16le` written to a named FIFO; opens blocking so the decode thread paces naturally), `SnapserverSupervisor` (spawns/restarts `snapserver` with N pipe streams; sub-step 3 launches it with `STREAM_POOL_SIZE = 16`, FIFOs `/tmp/audioshare-snapfifo-{k}`, stream ids `as-{k}`), and `StreamPool` (allocates slots to playing zones). Snapcast is an implementation detail behind the `AudioSink` seam and the router.
- `audio/snapcast_control.rs` — `CommandConn` (persistent JSON-RPC 2.0 TCP client to `snapserver` on `127.0.0.1:1705`; sends `Server.GetStatus`, `Group.SetClients`, `Group.SetStream`) and `EventListener` (reads push notifications on the same socket; signals the reconciler). Both defined behind the `SnapcastControl` trait for mock-based unit testing.
- `audio/snapcast_router.rs` — `SnapcastRouter`: the engine's single seam into Snapcast. Owns the supervisor, control client, stream pool, and desired-state reconciler. `sink_for_zone(zone, dongle_ids)` allocates a pool slot (`no_free_stream` if all 16 in use) and fires a reconcile; `release_zone(zone)` frees the slot. `reconcile_now()` (idempotent; also triggered by `Client.OnConnect`/`OnDisconnect` notifications) issues `Group.SetClients` + `Group.SetStream` RPCs to converge `snapserver` to the desired topology — eliminating the need for any manual `Group.SetStream` workaround.
- `audio/decode.rs` — decode thread body: HTTP stream → Symphonia decode → Rubato resample/mix → `AudioOutput::write`
- `security.rs` — all cryptography (X25519, HKDF, AES-256-GCM)
- `session.rs` — holds the symmetric session key and `last_activity` timestamp
- `pairing.rs` — loads/creates `/etc/audio_share/pairing_secret.b64`; generates QR payload
- `audio/output.rs` — `AudioOutput`, the cpal-backed PCM output sink (see below)

**Audio output layer (`audio/output.rs`, KAN-18):**
- `AudioOutput` is the boundary the playback engine writes PCM into: it pushes
  interleaved `f32` samples via `AudioOutput::write(&[f32])`, and a cpal output
  stream drains them to the host's default device, emitting silence on underrun.
- **Library choice — cpal** (over rodio/ALSA): cpal is thin, pure-Rust, and
  builds on both macOS (CoreAudio, dev) and the Pi (ALSA backend). rodio layers
  a `Source`/decoder/mixer model on top of cpal that fights our own decode
  (KAN-21) + multi-room routing; raw ALSA is Linux-only and breaks macOS dev.
- **Threading:** cpal's `Stream` is `!Send` on macOS, so `AudioOutput` owns the
  stream on a dedicated `audio-output` thread and hands samples to it through a
  shared `Mutex<VecDeque<f32>>`. The buffer is intentionally simple — proper
  lock-free jitter/underrun buffering is KAN-23, behind this same `write` API.
- **Toolchain:** cpal's macOS build path (bindgen → coreaudio-sys) pulls crates
  that require `edition2024`, so a recent stable Rust (≥1.85; 1.96 in use) is
  required. The Pi target `armv7-unknown-linux-gnueabihf` needs ALSA dev headers
  (`libasound2-dev`) at build time.

**Two-phase key establishment (non-obvious):**
- `Session::new()` internally derives a session key via ECDH + HKDF (no salt) — this becomes the AES key for the persistent connection
- `Security::get_encrypted_session_key()` performs a *separate* ECDH with a fresh ephemeral key pair, derives a one-time transport key via HKDF **with the pairing secret as the salt**, then AES-GCM encrypts the session key for transit
- The pairing secret salt prevents MITM: only a device that scanned the QR code can derive the transport key and decrypt the session key

**Global server instance:** `MAIN_SERVER` in `server/server.rs` is a `lazy_static` global used by `connection.rs` to authenticate sessions. The `Server` instance created in `main.rs` is separate — session storage goes through `MAIN_SERVER`, not through the `Arc<Server>` in `main`.

**Dead code:** `rest_server/` (Warp HTTP + Spotify routes), `mdb.rs` (SQLite), `authentication.rs`, and `user.rs` are all unused in the current build. The Spotify OAuth and user management are handled entirely by the Go service.

### Go service (`site/`)

- Gorilla mux router with PostgreSQL (`lib/pq` + gorm)
- `DATABASE_URL` env var selects the database; defaults to `postgres://audioshare_user:admin@localhost:5432/audioshare`
- Routes: `POST /createUser`, `POST /authenticateUser`, `POST /spotifyAuth`
- `PORT` env var sets the listen port (default `:8080`)
- The iOS app currently hardcodes `192.168.68.61:8080` as the Go server address in `ConnectionManager.post()`

## Cross-project wire protocol

Changes here must be mirrored in `~/Documents/Audio Share/` (iOS).

**QR payload (server prints at startup, iOS scans):**
```json
{ "s": "<serial_number>", "ps": "<base64 32-byte pairing secret>" }
```
Generated by `pairing::qr_payload()`. iOS decodes this in `DeviceConnect` and stores the pairing secret in Keychain keyed by serial number.

**Handshake (iOS → server, plaintext JSON over TCP):**
```json
{ "public_key": "<base64 Curve25519 public key>" }
```

**Handshake response (server → iOS, plaintext JSON):**
```json
{ "data": { "uuid": "<session UUID>", "session": "<base64>" } }
```
`session` is `nonce(12) ‖ ciphertext ‖ server_ephemeral_public_key(32)`, base64-encoded. Server encodes in `Security::get_encrypted_session_key()`; iOS decodes in `Sec.getSessionData()`.

**Subsequent messages (iOS → server, AES-GCM then base64 over TCP):**
```json
{ "task": "<action>", "data": <payload>, "session_token": "<UUID>" }
```
Entire JSON is encrypted with the session key and base64-encoded. Server decrypts via `Security::decrypt_data()`, then validates `session_token` in `Connection::authenticate_message()`.

The `task` field is parsed into `commands::Task` and routed by `commands::dispatch()` (called from `Connection::handle_task()`). Recognized tasks: `play`, `pause`, `stop`, `seek`, `volume`, `list_outputs`, `list_zones`, `create_zone`, `delete_zone`, `rename_zone`, `set_zone_outputs`, `list_sources`. Anything else is `Unknown`. (`list_outputs`, `list_zones`, and `list_sources` are special-cased in `handle_task` — not `dispatch` — because they need the connection to push; see the push messages below.)

**Zone CRUD tasks (hub-side shipped, iOS mirror pending own spec):**
- `create_zone` — `data: { "name": "<display name>" }` → `ENGINE.create_zone(name)`; success `data: { "zone": "<new-id>" }`.
- `delete_zone` — `data: { "zone": "<id>" }` → `ENGINE.delete_zone(zone)`; stops any playback and frees the pool slot. Member dongles remain reachable via their own auto-zones (keyed by dongle id), which are independent entries and are not removed; errors: `unknown_zone`.
- `rename_zone` — `data: { "zone": "<id>", "name": "<new name>" }` → `ENGINE.rename_zone(zone, name)`; errors: `unknown_zone`.
- `set_zone_outputs` — `data: { "zone": "<id>", "outputs": ["<output-id>", ...] }` → `ENGINE.set_zone_outputs(zone, outputs)`; enforces all-dongle or all-local (never mixed); errors: `unknown_zone`, `unknown_output`, `mixed_zone_unsupported`.

`play` carries the stream URL and an **optional target `zone`** (default `"default"` when omitted) in its payload: `{ "task": "play", "data": { "url": "<http stream url>", "zone": "kitchen" }, "session_token": "<UUID>" }`. `dispatch()` reads the zone and calls `audio::engine::ENGINE.play(zone, url)`, which resolves the zone's online outputs (opening the local cpal device lazily via `ensure_local` on first play) and spawns a per-zone decode thread (`audio::decode::stream_url_to_output`: HTTP fetch → Symphonia decode → Rubato resample → the zone's `AudioSink`). `stop` carries the same optional `zone` and calls `ENGINE.stop(zone)`. `pause`/`seek`/`volume` are still acknowledged as `not_yet_implemented` stubs.

**Task response (server → iOS, AES-GCM then base64 over TCP):**
```json
{ "status": "ok" | "error", "task": "<echoed task>", "data": <payload?>, "error": "<code?>" }
```
`status` is `ok` for an accepted command, `error` otherwise. `data`/`error` are omitted when absent. Error codes so far: `unsupported_task` (unknown `task`), `missing_task` (no `task` field), `missing_url` (`play` with no `data.url`), `playback_failed` (`play` could not open the audio device / spawn the decode thread), `unknown_zone` (`play` targeted a zone the engine doesn't know; also `delete_zone`/`rename_zone`/`set_zone_outputs` with an unknown id), `zone_has_no_outputs` (`play`'s target zone has no online outputs), `no_free_stream` (`play` on a dongle zone when all `STREAM_POOL_SIZE = 16` pool slots are in use — hub-side shipped, iOS mirror pending own spec), `mixed_zone_unsupported` (`set_zone_outputs` or `play` attempted to mix local and dongle outputs in one zone — hub-side shipped, iOS mirror pending own spec), `unknown_output` (`set_zone_outputs` referenced an output id that is not in the registry — hub-side shipped, iOS mirror pending own spec). The response is encrypted with the session key via `Security::encrypt_data()` (mirrors `decrypt_data`: `nonce(12) ‖ ciphertext`, base64). KAN-19 formalizes this protocol and KAN-24 expands the error taxonomy.

**Wire framing (server → iOS runtime messages):** every encrypted runtime message the server writes after the handshake — task responses **and** the unsolicited `outputs` push below — is **newline-terminated** (`base64\n`). Multiple messages multiplex on the one socket, and base64 (STANDARD, padded) never contains `\n`, so the client splits on it. `Connection::send_encrypted` adds the `\n`; iOS accumulates bytes and splits in `ConnectionManager.drainFrames`. The handshake response (`start_new_connection`) is **not** framed — iOS reads it separately via `sendAndReceive` before the receive loop starts.

**Live speaker list (`outputs`) — server → iOS push.** The hub maintains the set of playable targets (its own local output + attached dongles) and pushes the full list whenever it changes. The `Connection` subscribes to `audio::engine::OUTPUTS_CHANGED` (a `tokio::sync::broadcast` the engine fires from `register_dongle`/`dongle_offline`) and sends an `outputs` message on connect and on every tick; the client may also pull it on demand by sending `{ "task": "list_outputs" }`:
```json
{ "status": "ok", "task": "outputs",
  "data": { "outputs": [ { "zone": "default", "name": "Hub", "online": true },
                         { "zone": "<dongle-uuid>", "name": "Kitchen", "online": false } ] } }
```
`zone` is what the client puts in a `play`/`stop` task; the hub is always present first as the synthesized `{ zone: "default", name: "Hub" }` (built by `Engine::list_targets`, which maps the `"local"` output id to the `"default"` zone). An attached-but-unreachable dongle appears with `online: false`. On iOS, `ConnectionManager`'s receive loop decrypts each frame, and `Library`'s speaker menu (backed by `SpeakerListStore`) renders the hub as the default selection and offline dongles as disabled rows.

**Live zone list (`zones`) — server → iOS push (hub-side shipped, iOS mirror pending own spec).** On the same `OUTPUTS_CHANGED` trigger, the hub also sends a `zones` message carrying full zone membership and playback state, so a future grouping UI can render it. The client may also pull it on demand by sending `{ "task": "list_zones" }`:
```json
{ "status": "ok", "task": "zones",
  "data": { "zones": [
    { "zone": "default", "name": "Hub", "outputs": ["local"], "playing": false },
    { "zone": "<uuid>", "name": "Upstairs", "outputs": ["<d1>","<d2>"], "playing": true } ] } }
```
Each entry's `zone` is the id to use in `play`/`stop`/`delete_zone`/`rename_zone`/`set_zone_outputs`. `outputs` lists the `OutputId`s of member outputs. `playing` is `true` when a decode pipeline is active on that zone. The flat `outputs` push above is retained for backward compatibility with the shipped iOS speaker list; `zones` is additive.

**Live AirPlay source list (`sources`) — server → iOS push (hub-side shipped, iOS mirror pending own spec).** On `SOURCES_CHANGED` (fired by the engine on session begin/end, reroute, or detach — alongside `OUTPUTS_CHANGED`), the hub sends a `sources` message carrying only currently active AirPlay sessions; an idle receiver (no session) is invisible here (the client already has it from `zones`). The client may also pull it on demand by sending `{ "task": "list_sources" }`:
```json
{ "status": "ok", "task": "sources",
  "data": { "sources": [
    { "source": "<home-zone-id>", "name": "Kitchen",
      "dest_zone": "<zone-id>", "active": true, "routed": true } ] } }
```
`source` is the home zone's id (stable handle for that receiver); `dest_zone` is the current routing target (the home zone by default; moves on reroute). `active` is `true` when a session is live. `routed` is `true` when the source's PCM is being written to a sink (versus connected-but-unrouted). Track metadata fields (`client`, `title`, `artist`, `album`, `art_id`) are added in Slice 3. `SOURCES_CHANGED` is a `tokio::sync::broadcast` parallel to `OUTPUTS_CHANGED`; connections subscribe to both.
