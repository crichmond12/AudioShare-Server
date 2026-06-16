# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Companion Project

The iOS client lives at `~/Documents/Audio Share/` (Swift/SwiftUI). This repo is the server. The two must stay in sync on the wire protocol and QR payload format described at the bottom of this file.

## Product Vision & Roadmap

**What Audio Share is:** small network devices (this server, on a Raspberry Pi) that plug into ordinary (non-smart) speakers and make them wireless. A user picks music on their phone and plays it to any speaker on the network. This server is the device that actually drives the speaker.

**Core differentiator — independent multi-room:** the user can send *different* music to *different* speakers at the same time, each stream potentially from a *different* platform. So the server must support per-output (per-zone) playback: a device/output registry, zone grouping, and routing an independent stream to each output. Grouping outputs for *synchronized* playback of the same song (Sonos-style) is also a goal and is the harder case — it needs clock sync / latency alignment across outputs.

**Streaming platforms (planned):** Spotify first, then Apple Music, YouTube Music, Pandora. Licensing differs per platform (Spotify/Apple Music generally forbid raw-audio capture). The design uses a **per-source play-mode**: some sources play *server-side* here (the server fetches/streams and outputs audio — e.g. a librespot/Spotify-Connect path), others *relay* from the phone (the phone sends encoded bytes that the server buffers and outputs — for local/web audio).

**Current state (as of 2026-06):** the networking + security foundation is solid — TCP server, the two-phase X25519/HKDF/AES-GCM handshake, sessions, mDNS, and Go-side accounts/Spotify OAuth. But there is **no audio output at all** (no audio libraries imported), the connection loop authenticates messages then **drops the `task` field without dispatching it**, and there is no multi-output/zone concept, no streaming-source playback, and no tests. Dead code exists (`rest_server/`, `mdb.rs`, `authentication.rs`, `user.rs`). These gaps are tracked as epics/tasks in Jira project **KAN**. Treat message dispatch, server-side audio output, and per-zone routing as the central near-term work.

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
- `server/commands.rs` — parses the `task` field into `Task` and dispatches it (currently stubs; KAN-20 wires the engine)
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

The `task` field is parsed into `commands::Task` and routed by `commands::dispatch()` (called from `Connection::handle_task()`). Recognized tasks: `play`, `pause`, `stop`, `seek`, `volume`. Anything else is `Unknown`. The playback engine does not exist yet (KAN-18/20/21), so recognized tasks are currently acknowledged as `not_yet_implemented` stubs; KAN-20 wires the real engine into `dispatch()`.

**Task response (server → iOS, AES-GCM then base64 over TCP):**
```json
{ "status": "ok" | "error", "task": "<echoed task>", "data": <payload?>, "error": "<code?>" }
```
`status` is `ok` for an accepted command, `error` otherwise. `data`/`error` are omitted when absent. Error codes so far: `unsupported_task` (unknown `task`), `missing_task` (no `task` field). The response is encrypted with the session key via `Security::encrypt_data()` (mirrors `decrypt_data`: `nonce(12) ‖ ciphertext`, base64). KAN-19 formalizes this protocol and KAN-24 expands the error taxonomy; the iOS client does not yet consume these responses.
