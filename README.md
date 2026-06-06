# Audio Share — Server

A self-hosted Rust server that enables secure, encrypted audio sharing over a local network. Designed to run on a Raspberry Pi or home server, it broadcasts itself via mDNS so the companion iOS app can discover and connect to it automatically.

> 📱 **iOS Client:** [Audio Share iOS](https://github.com/YOUR_USERNAME/audio-share-ios)

---

## Features

- **Automatic device discovery** via mDNS (Bonjour) — no IP address entry required
- **End-to-end encryption** using X25519 Diffie-Hellman key exchange, HKDF key derivation, and AES-256-GCM
- **Session management** with per-client session keys and UUID-based tracking
- **Spotify OAuth token swap** endpoint so the iOS app can authenticate without exposing client secrets
- **SQLite** database for user and auth key persistence
- **QR code pairing** support for first-time device connection
- **Docker** support for containerized deployment
- Two independently runnable services: `audioshare_device` and `audioshare_site`

---

## Tech Stack

| Layer | Technology |
|---|---|
| Language | Rust (2021 edition) |
| Async runtime | Tokio |
| HTTP framework | Warp |
| Database | SQLite via rusqlite |
| Service discovery | mdns-sd (Bonjour/_tcp) |
| Encryption | x25519-dalek, AES-GCM, HKDF / SHA-256, ring |
| Serialization | serde / serde_json |
| Containerization | Docker |

---

## Architecture

```
┌─────────────────────────────────┐
│         Audio Share Server      │
│                                 │
│  ┌─────────────┐  ┌──────────┐  │
│  │  TCP Device │  │  REST    │  │
│  │  Server     │  │  Server  │  │
│  │  :50505     │  │ (Warp)   │  │
│  └──────┬──────┘  └────┬─────┘  │
│         │              │        │
│  ┌──────▼──────────────▼─────┐  │
│  │     mDNS Broadcast        │  │
│  │   _audioshare._tcp        │  │
│  └───────────────────────────┘  │
│  ┌───────────────────────────┐  │
│  │     SQLite Database       │  │
│  └───────────────────────────┘  │
└─────────────────────────────────┘
```

**Connection handshake flow:**
1. Server broadcasts via mDNS on the local network
2. iOS client discovers the service and initiates a TCP connection
3. Client sends its X25519 public key
4. Server performs ECDH, derives a shared session key via HKDF using the pairing secret as the salt
5. All subsequent communication is encrypted with AES-256-GCM

---

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (1.75+)
- SQLite (bundled via `rusqlite` — no separate install needed)

### Build & Run

```bash
# Clone the repo
git clone https://github.com/YOUR_USERNAME/audio-share-server.git
cd audio-share-server

# Run in development
cargo run

# Build for release (e.g., for Raspberry Pi cross-compilation)
cargo build --release
```

### Running with the shell script

```bash
# Run both services
./run.sh

# Run with verbose output
./run.sh -v

# Run only the device server
./run.sh device

# Run only the site (REST) server
./run.sh site
```

### Deploy to Raspberry Pi

```bash
# Cross-compile and copy binary to Pi
./to_pi.sh
```

### Docker

```bash
docker build -t audio-share-server .
docker run audio-share-server
```

---

## Database Migrations

Migrations live in `migrations/` and are plain SQL files. Run them in order:

```bash
python3 migrations/migrate.py
```

---

## Project Structure

```
audio_share/
├── src/
│   ├── main.rs              # Entry point — spawns device & broadcast tasks
│   ├── server/
│   │   ├── server.rs        # Core server: session map, task orchestration
│   │   ├── connection_server.rs  # TCP listener & encrypted handshake
│   │   ├── broadcast.rs     # mDNS service registration
│   │   └── connection.rs    # Per-client connection handler
│   ├── rest_server/
│   │   ├── server.rs        # Warp HTTP routes
│   │   └── spotify_routes.rs # Spotify OAuth token swap
│   ├── security.rs          # X25519 / HKDF / AES-GCM helpers
│   ├── pairing.rs           # Pairing secret load/create and QR payload generation
│   ├── session.rs           # Session key & lifetime tracking
│   ├── mdb.rs               # SQLite access layer
│   ├── json_structs/        # Request/response types
│   └── errors/              # Custom error types
├── migrations/              # SQL schema migrations
├── Dockerfile
├── run.sh                   # Launch script
└── to_pi.sh                 # Raspberry Pi deploy script
```

---

## Security Design

All client-server communication is encrypted end-to-end:

- **Key exchange:** X25519 Elliptic Curve Diffie-Hellman (ephemeral keys per session)
- **Key derivation:** HKDF with SHA-256, salted with a persistent 32-byte pairing secret — prevents MITM attacks by ensuring only a device that physically scanned the QR code can derive the session key
- **Symmetric encryption:** AES-256-GCM (authenticated encryption)
- **Session isolation:** Each client gets a unique UUID-keyed session with its own derived key
- **Pairing secret:** Generated once on first run, stored at `/etc/audio_share/pairing_secret.b64`, and embedded in the QR code displayed at startup

---

## License

MIT
