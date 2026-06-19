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

**Current state (as of 2026-06):** networking + security foundation is solid — TCP server, the two-phase X25519/HKDF/AES-GCM handshake, sessions, mDNS. **Phase 1 (first end-to-end audio path) is done:** a `play` task with a stream URL now drives a real engine (`audio/player.rs` + `audio/decode.rs`) that fetches a DRM-free HTTP internet-radio stream, decodes (Symphonia, mp3/aac), resamples (Rubato), and plays it through the `audio/output.rs` cpal sink; `stop` halts it. Still single-zone with no multi-output/zone concept (phase 2) and minimal buffering (KAN-23). Dead code exists (`rest_server/`, `mdb.rs`, `authentication.rs`, `user.rs`). The Go service currently does accounts + Spotify OAuth — under the pivot, server-side Spotify OAuth is no longer needed and the account model itself may become optional (pairing is the security boundary); revisit before investing further there. These gaps are tracked in Jira project **KAN**, which will be reconciled with the pivot later.

### Build plan (ordered)

Sequenced as vertical slices — each phase ends at something demoable. Jira (project **KAN**) will be reconciled with this plan later; the phase numbers below are not ticket IDs.

1. **First end-to-end audio path.** Wire `commands::dispatch()` into a real playback engine; finish the `AudioOutput` drain; add a decode step; play **internet radio** (HTTP stream → decode → cpal). Goal: phone says "play `<url>`", speaker makes sound. Proves the whole pipeline and is the gate for everything else (and for any demo video).
2. **Independent multi-room.** Per-output/zone registry + routing so each output plays its own independent stream. This is the headline feature.
3. **Synchronized multi-room via Snapcast.** Integrate/supervise Snapcast for grouped, time-aligned playback instead of a hand-rolled clock.
4. **Receiver protocols.** AirPlay 2 receive (`shairport-sync`) so any iPhone app can push audio with zero licensing exposure; Spotify Connect (`librespot`) as an optional plugin.
5. **More DRM-free sources.** Podcasts (RSS), Subsonic/Jellyfin client, phone-relayed local files.
6. **Product & portfolio polish.** Flashable image / one-command installer, zero-config onboarding, jitter/underrun buffering, reconnect resilience, tests; open-source hygiene (README, architecture diagram, demo video, license). Optional monetization: pre-flashed images / pro tier / donations.

## Two Services

This project produces two separate binaries that run together on a Raspberry Pi:

- **`audioshare_device`** — Rust: TCP server (port 50505) + mDNS broadcast. This is the primary service the iOS client connects to.
- **`audioshare_site`** — Go: HTTP REST API (default port 8080) backed by PostgreSQL. Handles user accounts and Spotify OAuth.

`run.sh` expects both pre-built binaries to exist in the working directory under exactly those names.

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
- `server/connection_server.rs` — TCP listener, handshake initiation, serial number read
- `server/broadcast.rs` — mDNS registration as `_audioshare._tcp.local.` on port 50505
- `server/connection.rs` — per-client loop; validates `session_token` UUID on every message
- `server/commands.rs` — parses the `task` field into `Task` and dispatches it; `play`/`stop` drive the playback engine, the rest are stubs
- `audio/player.rs` — `Player` + global `PLAYER`: owns the `AudioOutput` and the single in-flight decode thread; `play(url)`/`stop()`
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

The `task` field is parsed into `commands::Task` and routed by `commands::dispatch()` (called from `Connection::handle_task()`). Recognized tasks: `play`, `pause`, `stop`, `seek`, `volume`. Anything else is `Unknown`.

`play` carries the stream URL in its payload and drives the real playback engine: `{ "task": "play", "data": { "url": "<http stream url>" }, "session_token": "<UUID>" }`. `dispatch()` calls `audio::player::PLAYER.play(url)`, which opens the output device lazily and spawns a decode thread (`audio::decode::stream_url_to_output`: HTTP fetch → Symphonia decode → Rubato resample → `AudioOutput`). `stop` calls `PLAYER.stop()`. `pause`/`seek`/`volume` are still acknowledged as `not_yet_implemented` stubs.

**Task response (server → iOS, AES-GCM then base64 over TCP):**
```json
{ "status": "ok" | "error", "task": "<echoed task>", "data": <payload?>, "error": "<code?>" }
```
`status` is `ok` for an accepted command, `error` otherwise. `data`/`error` are omitted when absent. Error codes so far: `unsupported_task` (unknown `task`), `missing_task` (no `task` field), `missing_url` (`play` with no `data.url`), `playback_failed` (`play` could not open the audio device). The response is encrypted with the session key via `Security::encrypt_data()` (mirrors `decrypt_data`: `nonce(12) ‖ ciphertext`, base64). KAN-19 formalizes this protocol and KAN-24 expands the error taxonomy; the iOS client does not yet consume these responses.
