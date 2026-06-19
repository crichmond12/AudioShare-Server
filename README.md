# Audio Share — Server

Open, self-hostable software that turns any ordinary (non-smart) speaker or amp into a networked, **multi-room audio endpoint**. You flash or install it yourself on a Raspberry Pi (or similar Linux device); it broadcasts itself via mDNS so the companion iOS app can discover it, pair over an end-to-end encrypted channel, and play audio to it. No cloud, no account on someone else's servers — legally, this is just a speaker.

> 📱 **iOS Client:** [Audio Share iOS](https://github.com/YOUR_USERNAME/audio-share-ios)

This is the **server** — the device that actually drives the speaker. It is distributed as downloadable software (the Volumio / moOde / Home Assistant model), not as manufactured hardware.

---

## What it is

Audio Share is an **open audio endpoint that other apps play *to***, plus a player for open / DRM-free sources. Rather than trying to *be* a streaming service (which is not viable for a self-hosted product — the big platforms forbid raw-audio capture and their DRM only decrypts inside their own players), it sidesteps licensing entirely by being a speaker on your network.

**Two ways audio reaches a speaker:**

- **Sources the device plays itself** (DRM-free, the device holds the encoded bytes): internet radio, podcasts (RSS), self-hosted libraries (Subsonic/Navidrome, Jellyfin, Plex), and local / phone-relayed files. These are the legal core.
- **Receiver protocols** (the phone's *own* app streams to the device): AirPlay 2 (via `shairport-sync`), optionally Spotify Connect (via `librespot`) and Chromecast. You authenticate Spotify / Apple Music on your *own* phone and push to the device — zero licensing exposure. Gray-area integrations like `librespot` ship as optional, user-installed plugins, never bundled.

**Headline feature — multi-room:** send *different* audio to *different* speakers at once (independent per-zone playback), and group outputs for *synchronized* playback of the same source. Synchronized multi-room is built on **[Snapcast](https://github.com/badaix/snapcast)** rather than a hand-rolled clock.

---

## Features

- **Automatic device discovery** via mDNS (Bonjour) — no IP entry, the iOS app finds the server on the LAN
- **QR-code device pairing** — a 32-byte pairing secret in the QR binds the encrypted session to the physically-paired device
- **End-to-end encryption** — X25519 key exchange, HKDF key derivation, AES-256-GCM authenticated encryption
- **Real audio pipeline** — HTTP stream → decode (Symphonia, mp3/aac) → resample (Rubato) → playback (cpal) for DRM-free internet radio, with `play` / `stop` driven from the phone
- **Multi-room routing** — per-zone output registry so each zone plays its own independent stream (synchronized grouping via Snapcast is in progress)
- **Local-network only** — no cloud dependency; audio and control stay on your network

---

## Current state

The project is built as ordered vertical slices, each ending at something demoable:

| Phase | Goal | Status |
|---|---|---|
| 1 | First end-to-end audio path — phone says "play `<url>`", speaker plays internet radio | ✅ Done |
| 2 | Independent multi-room — per-zone registry + routing | 🚧 Scaffolding in (one `default` zone → local output) |
| 3 | Synchronized multi-room via Snapcast | 🚧 Building blocks in (`SnapcastSink` + supervisor), not yet wired into the engine |
| 4 | Receiver protocols — AirPlay 2 (`shairport-sync`), Spotify Connect plugin | ⏳ Planned |
| 5 | More DRM-free sources — podcasts, Subsonic/Jellyfin, phone-relayed files | ⏳ Planned |
| 6 | Product & portfolio polish — flashable image / one-command installer, buffering, reconnect, docs | ⏳ Planned |

---

## Tech Stack

| Layer | Technology |
|---|---|
| Language | Rust (recent stable, **1.85+** — cpal's macOS build path pulls edition-2024 deps) |
| Async runtime | Tokio |
| Audio output | [cpal](https://github.com/RustAudio/cpal) (CoreAudio on macOS for dev, ALSA on the Pi) |
| Decode | [Symphonia](https://github.com/pdeljanov/Symphonia) (mp3, aac, isomp4) |
| Resampling | [Rubato](https://github.com/HEnquist/rubato) |
| HTTP stream fetch | reqwest (blocking, rustls) |
| Synchronized multi-room | [Snapcast](https://github.com/badaix/snapcast) (external `snapserver`, supervised) |
| Service discovery | mdns-sd (`_audioshare._tcp`) |
| Encryption | x25519-dalek, aes-gcm, hkdf / sha2, ring |
| Serialization | serde / serde_json |

---

## Two services

Two separate binaries that run together on the Pi:

- **`audioshare_device`** — Rust: TCP server (port **50505**) + mDNS broadcast. The primary service the iOS client connects to and the one that drives the speaker.
- **`audioshare_site`** — Go: HTTP REST API (default `:8080`) backed by PostgreSQL, for user accounts and (legacy) Spotify OAuth.

> ⚠️ Under the endpoint pivot, server-side Spotify OAuth is no longer needed and the account model may become optional — pairing is the real security boundary. The Go service is retained for now but is a candidate for removal.

---

## Architecture

```
┌──────────────────────────── audioshare_device (Rust) ────────────────────────────┐
│                                                                                   │
│   iOS app ──TCP :50505──▶ ConnectServer ──▶ Connection (per-client, session auth) │
│       ▲                        │                        │                         │
│       │                        ▼                        ▼                         │
│   mDNS  ◀──── Broadcast    Security (X25519 /      commands::dispatch()           │
│  _audioshare._tcp          HKDF / AES-GCM)         (play / stop / …)              │
│                                                         │                         │
│                                                         ▼                         │
│                                                  audio::Engine                    │
│                                              (one decode thread per zone)         │
│                                                         │                         │
│                              OutputRegistry  ◀──────────┘                         │
│                            (zone → AudioSink)                                      │
│                              │            │                                       │
│                              ▼            ▼                                       │
│                       AudioOutput   SnapcastSink ──FIFO──▶ snapserver (external)   │
│                        (cpal)                                                     │
└───────────────────────────────────────────────────────────────────────────────────┘
```

**Connection handshake flow:**
1. Server broadcasts via mDNS on the local network
2. iOS client discovers the service and opens a TCP connection
3. Client sends its X25519 public key
4. Server performs ECDH and derives the session key via HKDF, using the **pairing secret as the salt** — so only a device that scanned the QR can decrypt the session key (MITM protection)
5. All subsequent messages are AES-256-GCM encrypted and validated against the session UUID

**Audio path (`play`):** the phone sends `{ "task": "play", "data": { "url": "<stream>", "zone": "kitchen" } }`. `dispatch()` resolves the target zone's online outputs and spawns a per-zone decode thread: HTTP fetch → Symphonia decode → Rubato resample/mix → the zone's `AudioSink` (the local cpal device today; a Snapcast input or a network dongle later). `stop` halts the zone.

---

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) **1.85+** (1.96 in use)
- On the Pi (`armv7-unknown-linux-gnueabihf`): ALSA dev headers — `sudo apt install libasound2-dev`
- (Optional, for synchronized multi-room) `snapserver` / `snapclient`

> **macOS dev caveat:** the binary reads `/proc/cpuinfo` for the Pi serial number and will exit on macOS. Developing the TCP / security logic on macOS requires stubbing `get_serial_number()` or running on Linux.

### Build & Run

```bash
# Clone
git clone https://github.com/YOUR_USERNAME/audio-share-server.git
cd audio-share-server

# Rust device server (development)
cargo run

# Release build and copy to the Pi (host alias "pi")
./compile.sh

# Go site server
cd site && go build -o audioshare_site && ./audioshare_site
```

### Deploy to the Raspberry Pi

```bash
./to_pi.sh           # build + scp both binaries
./to_pi.sh device    # Rust only
./to_pi.sh site      # Go only
```

### Run on the Pi (expects pre-built binaries present)

```bash
./run.sh             # both, backgrounded with logging
./run.sh -v          # both, foreground verbose
./run.sh device      # Rust only
```

### Database migrations (Go / PostgreSQL side)

```bash
python3 migrations/migrate.py
```

---

## Project Structure

```
audio_share/
├── src/
│   ├── main.rs                 # Entry point — spawns the device & broadcast tasks
│   ├── server/
│   │   ├── server.rs           # Global server instance, session map
│   │   ├── connection_server.rs # TCP listener & encrypted handshake
│   │   ├── broadcast.rs        # mDNS service registration
│   │   ├── connection.rs       # Per-client loop; validates session on each message
│   │   └── commands.rs         # Parses `task`, reads target `zone`, drives the engine
│   ├── audio/
│   │   ├── engine.rs           # Engine + per-zone decode threads; play(zone, url) / stop(zone)
│   │   ├── registry.rs         # OutputRegistry — zone → online AudioSink
│   │   ├── sink.rs             # AudioSink trait (decode/output boundary)
│   │   ├── output.rs           # cpal-backed local PCM output
│   │   ├── decode.rs           # HTTP → Symphonia decode → Rubato resample → sink
│   │   └── snapcast.rs         # SnapcastSink + snapserver supervisor (Change 5)
│   ├── security.rs             # X25519 / HKDF / AES-GCM
│   ├── pairing.rs              # Pairing secret load/create + QR payload
│   ├── session.rs              # Session key & last-activity tracking
│   └── json_structs/           # Request/response types
├── site/                       # Go service (accounts + legacy Spotify OAuth)
├── docs/                       # Design docs (e.g. multi-room-plan.md)
├── migrations/                 # SQL schema migrations (PostgreSQL)
├── run.sh / to_pi.sh / compile.sh
└── Cargo.toml
```

> **Note on dead code:** `rest_server/` (Warp + Spotify routes), `mdb.rs` (SQLite), `authentication.rs`, and `user.rs` are leftovers from the pre-pivot design and are not part of the current build. They are slated for removal.

---

## Security Design

All client–server communication is encrypted end-to-end:

- **Key exchange:** X25519 Elliptic-Curve Diffie-Hellman (ephemeral keys per session)
- **Key derivation:** HKDF/SHA-256, salted with a persistent 32-byte pairing secret — only a device that physically scanned the QR can derive the session key, which prevents MITM
- **Symmetric encryption:** AES-256-GCM (authenticated encryption)
- **Session isolation:** each client gets a unique UUID-keyed session with its own derived key
- **Pairing secret:** generated once on first run, stored at `/etc/audio_share/pairing_secret.b64`, and embedded in the QR shown at startup

---

## License

MIT
