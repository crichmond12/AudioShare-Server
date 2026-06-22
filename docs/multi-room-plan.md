# Audio Share — Multi-Room Architecture Plan

> Living design doc for the move from single-zone playback to the networked
> multi-room "hub + dongles" architecture. Read this first in any future
> session working on the audio engine. Cross-check against `CLAUDE.md` (the
> source of truth for protocol/state) — update both when reality changes.

---

## Context — why this plan exists

**The product vision (per the 2026-06 pivot).** Audio Share is open,
self-hostable software that turns a dumb speaker into a networked, multi-room
audio endpoint. A user installs the **hub** (this Rust service) on a Raspberry
Pi, their phone connects to the hub, and the hub plays audio to speakers around
the home. Legally we are *a speaker*, not a streaming service — audio reaches a
speaker either because (a) the hub plays a DRM-free source itself (internet
radio, podcasts, self-hosted libraries, phone-relayed files) or (b) a receiver
protocol (AirPlay 2, optional Spotify Connect) lets the phone's own app push to
us. Distribution is the Volumio/moOde model: **flashable software the user runs
on hardware they supply — not a hardware product we manufacture.**

**The headline feature is multi-room**, and that is what this plan builds:
- **Independent** per-zone playback — kitchen plays jazz while the bedroom
  plays a podcast, each its own stream (roadmap Phase 2).
- **Synchronized** groups — several speakers play the *same* source,
  time-aligned, via **Snapcast** (roadmap Phase 3).

**The "dongle" design the user is targeting.** The hub routes audio over WiFi
to small **networked receiver dongles** at each speaker (WiFi SBC + DAC →
RCA/3.5mm into the speaker). The user tells the hub "play this here," and the
hub streams to the right dongle(s), synced when grouped. In system terms a
dongle is a **networked audio client** — essentially a Snapcast client — shipped
as a flashable image the user puts on cheap hardware they own. This keeps us
inside the pivot (no FCC/CE/inventory). If the user ever decides to manufacture
and sell physical dongles, that is a *deliberate reversal* of the pivot to be
recorded as such — not assumed here.

**The problem this plan solves.** Today's engine bakes "single zone" into the
type system in three places. Multi-room is fundamentally about giving each of
those an identity: the **sink** becomes an interface (local device *or* remote
dongle), the **output** becomes a registry entry, and **playback** becomes
per-zone. This doc sequences that refactor so each step is independently
shippable and the risky network/Snapcast work lands last, behind a stable
interface.

---

## Current state (grounded in code, 2026-06)

Networking + security foundation is solid (TCP server on 50505, two-phase
X25519/HKDF/AES-GCM handshake, sessions, mDNS). **Phase 1 — first end-to-end
audio path — is done:** phone says `play <url>`, the Pi fetches a DRM-free HTTP
internet-radio stream, decodes (Symphonia), resamples (Rubato), and plays it out
the local cpal device; `stop` halts it.

The engine is **single-zone**, and three specific spots encode that:

1. **`Player` owns exactly one output and one pipeline** —
   `src/audio/player.rs:44-49`:
   ```rust
   struct PlayerInner {
       output: Option<Arc<AudioOutput>>,   // exactly one sink
       current: Option<Pipeline>,          // exactly one decode thread
   }
   ```
   Global `PLAYER` lazy_static at `player.rs:25-28`.

2. **The decode thread writes to a concrete local device** —
   `src/audio/decode.rs:38`: `stream_url_to_output(url, output: &AudioOutput, stop)`.
   It knows it's writing to a *cpal* `AudioOutput`, not "some sink." The
   resample/mix body drives entirely off `output.sample_rate()` /
   `output.channels()`.

3. **`play` takes no target** — `src/audio/player.rs:70` (`play(&self, url)`)
   and `src/server/commands.rs:51-66` (`PLAYER.play(url)`). No "where."

Supporting pieces that already have the right shape:
- `src/audio/output.rs` — `AudioOutput` is the cpal sink. Its public surface is
  exactly `write(&[f32])`, `sample_rate() -> u32`, `channels() -> u16`, all
  `&self`. This is already trait-shaped. Internals (dedicated `audio-output`
  thread owning the `!Send` cpal stream, `Mutex<VecDeque<f32>>` buffer) do not
  change in this plan.
- `src/server/commands.rs` — `Task` enum + `dispatch()`; `play`/`stop` are real,
  `pause`/`seek`/`volume` are `not_yet_implemented` stubs, unknown → error.
- Wire protocol + error taxonomy documented at the bottom of `CLAUDE.md`.

Out of scope here / known gaps: jitter/underrun buffering (KAN-23), the receiver
protocols (Phase 4), dead code (`rest_server/`, `mdb.rs`, `authentication.rs`,
`user.rs`).

---

## Target architecture

```
   Phone app
      │  "play <url> at <zone>"
      ▼
   ┌──────────────────────────────────────┐
   │  HUB  (this Rust service, the Pi)     │
   │  • OutputRegistry  (every dongle +    │
   │      the local device, by id/name)    │
   │  • Engine: one pipeline PER ZONE      │
   │  • decode → AudioSink (trait)         │
   │  • Snapcast server (synced groups)    │
   └──────────────────────────────────────┘
      │ WiFi
      ├───────────────┬───────────────┐
      ▼               ▼               ▼
   Dongle A        Dongle B        Dongle C     each = AudioSink (NetworkSink/
   (Kitchen)       (Living rm)     (Bedroom)    SnapcastSink) → DAC → RCA → speaker
```

Key idea: **decode never knows whether PCM lands on the local speaker or a
dongle across WiFi.** Everything routes through an `AudioSink` trait. The local
cpal device is just one registered output among many.

---

## The changes (dependency order — each independently shippable)

### Change 1 — Extract an `AudioSink` trait (keystone, pure refactor)

Smallest change; unblocks everything. No behavior or protocol change.

- New `src/audio/sink.rs`:
  ```rust
  pub trait AudioSink: Send + Sync {
      fn sample_rate(&self) -> u32;
      fn channels(&self) -> u16;
      fn write(&self, samples: &[f32]);
  }
  ```
- `impl AudioSink for AudioOutput` — forwards to existing methods (they already
  match exactly; `output.rs` otherwise untouched).
- Retype `decode.rs:38` to `output: &dyn AudioSink`. The resample/mix body is
  unchanged because it only uses the trait methods.
- `player.rs` holds `Arc<dyn AudioSink>` instead of `Arc<AudioOutput>`.

Verify: existing tests pass; the ignored end-to-end radio test still plays.

### Change 2 — Output registry

An "output" becomes a first-class, named, locatable thing. The local device is
one entry; each dongle adds another when it registers.

- New `src/audio/registry.rs`:
  ```rust
  pub type OutputId = String;
  pub struct Output {
      pub id: OutputId,
      pub name: String,              // "Kitchen"
      pub sink: Arc<dyn AudioSink>,  // local cpal OR a NetworkSink
      pub online: bool,
  }
  pub struct OutputRegistry { outputs: Mutex<HashMap<OutputId, Output>> }
  // register(output), remove(&id), sink(&id) -> Option<Arc<dyn AudioSink>>, list()
  ```
- At startup, register the local cpal device as `id: "local"`.

### Change 3 — Zones + an `Engine` (replaces `Player`)

A **zone** is a named group of outputs sharing playback. `Player` becomes
`Engine`, holding the registry and one pipeline *per zone*.

- New `src/audio/engine.rs` (supersedes `player.rs`; keep the `Pipeline`
  stop-flag/join logic from `player.rs:30-42,73-97` essentially verbatim — it
  just operates on `zones[zone]` now):
  ```rust
  pub type ZoneId = String;
  struct ZonePlayback { outputs: Vec<OutputId>, current: Option<Pipeline> }
  pub struct Engine {
      registry: Arc<OutputRegistry>,
      zones: Mutex<HashMap<ZoneId, ZonePlayback>>,
  }
  // play(&self, zone: &ZoneId, url: &str) -> Result<(),String>
  // stop(&self, zone: &ZoneId)
  ```
  `play` resolves the zone's outputs → their sinks → wraps in a `FanOut` sink →
  stops the zone's existing pipeline → spawns a decode thread into the fan-out.
- `FanOut` implements `AudioSink` by writing to each member sink; reports a fixed
  canonical format (e.g. 48000/2) all sinks accept.
- Ship this step with only the local output registered and a single `"default"`
  zone — behavior is identical to today, but the structure is now multi-room.

> ⚠️ **Sync boundary.** `FanOut` gives *independent* per-zone playback (Phase 2)
> and only *loose* sync within a group — each dongle buffers independently and
> can drift/echo. For **tight** same-source sync (Phase 3) do **not** fan out in
> Rust: the grouped zone's sink becomes a single `SnapcastSink` feeding
> `snapserver`, and dongles run `snapclient`, which does sub-millisecond
> alignment. FanOut for independent zones; Snapcast for synced groups. Do not
> hand-roll clock sync.

#### Implementation notes for Changes 2 + 3 (locked in before coding)

These refine the sketches above with constraints found by reading the committed
code (`player.rs`, `commands.rs` tests):

1. **`Engine::new()` / `ENGINE` init must NOT open the audio device.** The
   device-free test `server::commands::tests::stop_dispatches_to_ok` calls
   `dispatch(Task::Stop)`, which triggers `ENGINE` lazy-init via `stop`. If init
   eagerly opened the cpal device it would panic on CI/macOS without audio
   hardware. So **preserve the lazy "open local device on first successful
   `play`"** behavior that `Player` has today (`player.rs:79-83`). Construction
   only builds the registry (empty of the device) and the zone map.
2. **Local device is opened + registered lazily via a private
   `Engine::ensure_local() -> Result<Arc<dyn AudioSink>, String>`** — idempotent:
   if `registry.sink("local")` exists return it, else `AudioOutput::new()?`,
   register it as `Output { id:"local", name:"Local", online:true }`, return the
   sink. `play` calls this only when the target zone includes `"local"`. This is
   the one place the device-open error (today's `playback_failed`) still surfaces.
3. **Single-sink zones bypass `FanOut`.** `FanOut` must report a fixed
   sample_rate/channels, but the local cpal device runs at its own native rate
   and `decode` resamples to `sink.sample_rate()`. Wrapping the lone local sink
   in a 48000/2 `FanOut` would mismatch the device and shift pitch. So: resolve
   the zone's online sinks; if exactly one, pass it through directly (identical
   to today's behavior); only wrap in `FanOut` when there are ≥2. The canonical
   shared-format reconciliation `FanOut` needs is genuinely a **Change 5**
   problem (network sinks negotiate a common format) — include the `FanOut` type
   now but it stays unconstructed until a second output exists.
4. **`commands.rs` hardcodes the `"default"` zone in this step** — no protocol
   change yet. `PLAYER.play(url)` → `ENGINE.play("default", url)`, `PLAYER.stop()`
   → `ENGINE.stop("default")`, import `ENGINE` instead of `PLAYER`. Reading the
   zone from `data.zone` is **Change 4**. Keep the existing Err→`playback_failed`
   mapping for now (the single `"default"` zone always resolves).
5. **Lock order:** `play` locks `zones` then (via `ensure_local`) `registry`;
   nothing locks them in the reverse order, so no deadlock. `registry` and
   `zones` are independent mutexes.
6. **`player.rs` is deleted**; `mod.rs` drops `pub mod player;` and adds
   `pub mod engine;` + `pub mod registry;`. The `Pipeline` struct + `shutdown`
   move into `engine.rs` essentially verbatim.
7. **New unit tests are device-free:** `registry.rs` tests use a dummy
   `AudioSink` impl (no cpal) to cover register / online-vs-offline `sink()` /
   remove. `engine.rs` can test `stop` on an empty/unknown zone without a device.
   The `play` success path stays device-gated (manual/ignored), as today.

### Change 4 — `play`/`stop` carry a target (wire protocol)

- `commands.rs` `dispatch()` reads an optional zone, defaulting so nothing
  breaks:
  ```rust
  let zone = data["zone"].as_str().unwrap_or("default").to_string();
  ENGINE.play(&zone, url)   // same ok/error arms as today
  ```
- Wire payload gains one field:
  `{ "task":"play", "data":{ "url":"...", "zone":"kitchen" }, "session_token":"..." }`
- New error codes: `unknown_zone`, `zone_has_no_outputs`.
- **Mirror in iOS** (`~/Documents/Audio Share/`) and update the protocol section
  of `CLAUDE.md`.

### Change 5 — Where the dongles plug in (network + Snapcast, lands last)

This is the first *real* second output and the biggest leap. Decode/engine are
untouched — everything lands behind the `AudioSink` trait and the registry.

**Locked architecture decision (2026-06): a custom dongle agent that *wraps*
Snapcast.** Rather than ship stock `snapclient` bare, the dongle runs our own
agent that uses Snapcast underneath for connection, clock-sync, and grouping.
This puts a seam at the dongle boundary — the same instinct as `AudioSink` on
the hub: **Snapcast becomes an implementation detail behind our agent + our
protocol**, so we can change/extend/replace the sync mechanism later without
touching the hub↔dongle contract. (This is how Volumio/moOde wrap underlying
audio daemons.) It costs us a dongle codebase to own, but buys flexibility,
control over identity/registration/UX, supervision, and portfolio value.

```
Hub (audio_share):  zones / OutputRegistry = SOURCE OF TRUTH
                      → programs snapserver (groups/streams) via its JSON-RPC API
                      → learns dongles via agent registration (our protocol)
                      → SnapcastSink (impl AudioSink) writes a zone's PCM into
                        the snapserver input FIFO
Dongle agent (custom, our protocol):  identity + zone assignment + supervises
                      snapclient (spawn/restart, WiFi reconnect, health report)
Snapcast:  audio transport + clock sync + low-level grouping  (impl detail)
```

**Two disciplines that keep this sound (do not violate):**
1. **The agent delegates, it does not reimplement.** It supervises `snapclient`
   and talks to `snapserver`; it must never try to do audio transport or clock
   sync itself. Keep it a control/registration/supervision layer — that's what
   preserves the "Snapcast is swappable" benefit instead of inheriting the hard
   problem.
2. **The hub's zone model is the single source of truth.** Snapcast has its own
   streams/groups/clients model; the hub *programs* Snapcast to match (zone
   membership → snapserver groups/streams via JSON-RPC). The agent handles
   identity + supervision; grouping is orchestrated hub-side. Do not let the two
   grouping models drift or fight.

**Repo / crate layout (settled):**
- **Hub-side code stays in `audio_share`:** `SnapcastSink` (`impl AudioSink`),
  snapserver supervision + JSON-RPC client, and the dongle registration listener
  (new networking parallel to `ConnectServer`, `src/server/connection_server.rs`)
  that calls `OutputRegistry::register(...)` / `set_online(false)` on connect /
  disconnect.
- **The custom dongle agent = a workspace crate inside `audio_share`** (e.g.
  `crates/dongle_agent` or a `[[bin]]`/workspace member), so it **shares Rust
  protocol types with the hub** and avoids the manual wire-sync drift we already
  fight with the iOS client. This is *the* reason it's not a separate repo.
- **The flashable dongle image = a separate packaging repo** (minimal Linux /
  Buildroot that boots, joins WiFi, auto-starts our agent which in turn runs
  `snapclient` + drives the DAC). That repo is build/packaging config, not
  application logic — the logic lives in the workspace crate above.

**Build order within Change 5 (sub-steps):**
1. Hub supervises `snapserver` + `SnapcastSink` writing one zone's PCM into it;
   verify with a stock `snapclient` on a laptop before any custom agent/image.
   **Building blocks landed (`src/audio/snapcast.rs`):** `SnapcastSink`
   (`impl AudioSink`; interleaved `f32` → `s16le` into a `snapserver` pipe FIFO,
   opened non-blocking + lazily so the decode thread never stalls waiting on
   snapserver, dropping on `ENXIO`/`WouldBlock`/broken-pipe like the local
   output's overrun handling) and `SnapserverSupervisor` (spawns a `snapserver`
   with one `pipe://…?mode=create&sampleformat=48000:16:2&codec=pcm` stream,
   restarts it on exit, kills on drop — no JSON-RPC yet; grouping is sub-step 3).
   Unit-tested device-free; the audio+sync path is the opt-in
   `audio::snapcast::tests::plays_to_snapcast_briefly` (`--ignored`, needs
   `snapserver`/`snapclient` + hardware). **Still open in sub-step 1:** these
   are not yet referenced by the engine — registering a Snapcast output into the
   `OutputRegistry` and routing a zone to it rides along with sub-step 2's
   registration work.
2. Custom dongle agent: registration protocol to the hub + `snapclient`
   supervision; hub registers it into `OutputRegistry`.
3. Hub programs snapserver groups/streams from zone membership (sync + grouping).
4. Flashable image (separate repo) bundling agent + snapclient.

Sub-step 1 proves the audio+sync path with zero custom dongle code, so the risky
custom-agent/image work is itself sequenced last.

#### Sub-step 2 — detailed plan (locked 2026-06)

The custom dongle agent + hub registration. Decisions below are settled; code
them in this order. Update `CLAUDE.md` (protocol/state source of truth) as each
commit lands — this doc holds the plan, `CLAUDE.md` holds the shipped reality.

**What it delivers — and the one honest limit.** A device running our **dongle
agent** is claimed by a hub through the app, registers itself, and becomes an
`OutputRegistry` entry; the agent supervises a `snapclient` pointed at the hub's
`snapserver`, so `play {url, zone:<dongle>}` makes sound at that dongle — our own
code on both ends. **Limit:** there is **one `snapserver` stream** in sub-step 2,
so every registered dongle is a `snapclient` of that one stream and they all play
the *same* audio (one synchronized group). Per-dongle **independent** routing
needs multiple streams + group assignment via snapserver JSON-RPC — that is
**sub-step 3**, by design. Sub-step 2 registers each dongle under its own output
id (forward-compatible) but the shared stream means routing to any dongle feeds
all of them until sub-step 3. Comment this loudly so it isn't mistaken for a bug.

**Locked decisions:**
1. **Workspace, minimal churn.** Keep the `audio_share` package at the repo root
   (`Cargo.toml` + `src/` unchanged so `compile.sh`/`to_pi.sh`/`run.sh` keep
   working) and add a `[workspace]` table to that same file:
   `members = [".", "crates/protocol", "crates/dongle_agent"]`. A root crate may
   also be a workspace root. New crates live under `crates/`; the hub and the
   agent both depend on `protocol` by path. This is *the* reason the agent is not
   a separate repo — shared Rust types, no wire drift (the problem we fight with
   iOS).
2. **`crates/protocol` — pure serde control types + framing.** Audio never flows
   here (Snapcast carries audio); this is registration/control only, which keeps
   its security surface small. Framing: **newline-delimited JSON** over TCP
   (`serde_json` + `\n`, tokio `BufReader::read_line`) — debuggable with `nc`.
   Messages:
   - `Dongle → Hub`: `Register { dongle_id, name }`.
   - `Hub → Dongle`: `Registered { snapserver_host, snapserver_port }`.
   - `App → Dongle`: `Assign { hub_host, hub_port }` + ack (the discovery flow).
   - Heartbeat/health and `Stop`/`Assign{zone}` are deferred (grouping is
     sub-step 3).
3. **Identity.** The dongle generates a UUID once and persists it (mirrors the
   hub's `pairing_secret.b64`) and sends a default `name` (its hostname). The hub
   uses `dongle_id` as the `OutputId`.
4. **App-mediated discovery (resolves multiple hubs on one LAN).** The dongle
   does **not** auto-pick a hub. Unassigned, it advertises via mDNS as
   `_audioshare-dongle._tcp` (id + name in TXT) and runs a small **assignment
   listener**. The app — already paired to one specific hub — browses for
   unassigned dongles, the user taps "add to this hub," and the app sends the
   dongle `Assign { hub_host, hub_port }`. The dongle persists that address and
   switches to the dongle→hub registration flow, reconnecting to *that* hub on
   every boot. A `--hub <ip>` CLI flag stays as a **dev-only** bring-up shortcut.
   This puts the "which hub" choice where the human + multi-hub knowledge are.
5. **Hub registration listener.** New `src/server/dongle_server.rs` parallel to
   `connection_server.rs`, on a **new port 50506**, spawned as a third
   `tokio::spawn` in `Server::start()` beside `ConnectServer` and `Broadcast`.
   On `Register`: `ENGINE.register_dongle(id, name)`, reply `Registered { hub LAN
   IP, 1704 }`, hold the connection; on EOF → `ENGINE.dongle_offline(id)`.
6. **Auth deferred.** The dongle channel is control-only on the user's own LAN
   with user-flashed devices, so it ships **unauthenticated** in sub-step 2.
   Recorded as a known gap; revisit if/when the threat model warrants it.
7. **Engine/registry wiring (turns sub-step 1's blocks live; drops
   `snapcast.rs`'s `#![allow(dead_code)]`).** `Engine` holds a
   `Mutex<Option<SnapserverSupervisor>>` + a shared `Arc<SnapcastSink>`.
   `ensure_snapcast()` is lazy, mirroring `ensure_local()` (construction still
   opens nothing). `register_dongle(id, name)` → `ensure_snapcast()`, register
   `Output { id, name, sink: shared snapcast sink, online:true }`, and
   **auto-create a zone `id → [id]`** so `play {zone:<dongle>}` works without
   zone-CRUD (a later protocol change). `dongle_offline(id)` → `set_online(false)`.

**Commit order (each independently shippable):**
- **2.1** Workspace + `protocol` crate. Pure refactor; `cargo test` stays green.
- **2.2** Hub listener + engine/registry wiring. Demo gate: register via `nc` +
  a stock `snapclient -h <hub>`, then `play {zone:<id>}` → sound. No agent code
  in this commit, so the hub path is proven before the risky network code.
- **2.3** ✅ **Landed.** `crates/dongle_agent`: a workspace binary crate sharing
  `audioshare_protocol` with the hub. Modules: `storage` (persisted UUID + name +
  assigned-hub address, atomic writes mirroring `pairing.rs`; `Storage::at` makes
  it testable), `assignment` (mDNS advertise as `_audioshare-dongle._tcp` +
  assignment listener on `DONGLE_ASSIGNMENT_PORT`; `await_assignment` returns the
  app-chosen hub), `registration` (`run_session`: connect → `Register` → read
  `Registered` → start snapclient → hold the connection as liveness), `supervisor`
  (`SnapclientSupervisor`, a copy of the `SnapserverSupervisor`
  spawn/monitor/restart/kill-on-drop pattern spawning `snapclient -h <host> -p
  <port> --hostID <dongle_id>`; the agent *delegates* — it never does transport or
  clock-sync itself). `main` resolves the hub as `--hub` flag (dev shortcut, also
  persisted) > persisted assignment > `await_assignment`, then reconnects on a
  fixed delay. Device-free unit tests for storage + arg building pass; the
  `snapclient`/network path is exercised by the demo gate, not CI. Demo gate: run
  the agent on a laptop, assign it (or `--hub`), `play {zone:<dongle>}` → sound.
- **2.4** ✅ **Landed.** Resilience + heartbeat + device-free tests.
  - *Reconnect backoff* (`crates/dongle_agent/src/main.rs`): the fixed 3 s retry
    is now exponential — `next_backoff` doubles `BACKOFF_BASE` (1 s) up to
    `BACKOFF_MAX` (30 s); a session that stayed up ≥ `BACKOFF_RESET_AFTER` (30 s,
    longer than the heartbeat timeout so a heartbeat-killed session doesn't count
    as healthy) resets the delay. `next_backoff` is pure + unit-tested.
  - *Heartbeat* (shared consts in `crates/protocol`): the dongle sends
    `DongleToHub::Heartbeat` every `HEARTBEAT_INTERVAL_SECS` (5 s) on a dedicated
    task (so the timed read is never cancelled mid-line — `read_line` isn't
    cancel-safe), and the hub replies `HubToDongle::Heartbeat`. Both ends read
    with a `HEARTBEAT_TIMEOUT_SECS` (15 s) timeout, so a WiFi dropout where TCP
    never delivers a FIN is caught within that window: the hub marks the output
    offline, the agent reconnects. Replaces relying solely on the held-open
    connection's EOF.
  - *Device-free tests:* protocol heartbeat round-trip
    (`crates/protocol`); a loopback register → heartbeat-reply → offline cycle
    against a `DongleServer` with a mock `DongleRegistrar`
    (`src/server/dongle_server.rs`, no `snapserver`/hardware); and the agent's
    `Assign`→`Assigned` handshake over loopback
    (`crates/dongle_agent/src/assignment.rs`). The `snapclient`/`snapserver` audio
    path stays exercised by the demo gate, not CI.
  - *Refactor:* `DongleServer` reaches the engine through a `DongleRegistrar`
    trait (production `EngineRegistrar` forwards to `ENGINE`) so connection
    handling is testable with a mock.
  - **Still deferred to later sub-steps:** auth on the dongle channel; iOS
    scan-and-assign (2.5); hub-driven per-dongle grouping (sub-step 3).
- **2.5** ✅ **Landed (iOS side).** iOS app: scan `_audioshare-dongle._tcp`,
  assign to the current hub — **cross-project**, in `~/Documents/Audio Share/`.
  Three new files: `Managers/DongleDiscoveryManager.swift` (an `NWBrowser` +
  `NetService` browse of `_audioshare-dongle._tcp`, parallel to the hub's
  `ServiceDiscoveryManager`; resolves each dongle's TXT `id`/`name` + IPv4 into a
  published `[DiscoveredDongle]`, upserted by `id`, pruned as adverts leave),
  `Managers/DongleAssignmentManager.swift` (speaks the agent's assignment
  handshake — connects to the dongle's listener on `DONGLE_ASSIGNMENT_PORT`
  (50507), sends one newline-delimited `AppToDongle::Assign { hub_host, hub_port
  = HUB_REGISTRATION_PORT 50506 }`, awaits `DongleToApp::Assigned { dongle_id }`;
  `hub_host` is the hub IP the app is paired to via
  `ConnectionManager.getLocalServerIPAddress()`), and
  `Controllers/Library/DongleListView.swift` (a `List` of discovered dongles with
  a per-row "Add to this hub" button + status, reachable from `Library` via a
  `NavigationLink` "Add a Speaker"). The assignment wire types mirror
  `audioshare_protocol`'s `AppToDongle`/`DongleToApp` (tagged JSON, `type` field)
  — the shared-crate discipline doesn't cover the iOS client, so this is the
  hand-mirrored boundary to keep in sync. Replaces the free-text Zone field's job
  of *finding* a dongle (the field stays as the way to *target* one until a
  playback picker lands). Not built: auth on the assignment/registration channels;
  hub-driven per-dongle grouping (sub-step 3). **Untested on-device** — the dev
  box has only Command Line Tools (no `xcodebuild`); needs a build + a real
  agent on the LAN to verify the browse + handshake.

#### Bring-up note — the dongle agent's mDNS advert is invisible on macOS (2026-06-20)

Running the dongle agent on a **Mac** for dev, its mDNS advert
(`_audioshare-dongle._tcp` via the pure-Rust `mdns-sd` crate) is **not
discoverable** by `dns-sd` or the iOS app, even though the agent logs
"Unassigned. Advertising as …". Cause: macOS already runs Apple's
`mDNSResponder` (and often a second responder — Chrome/Google was holding UDP
5353 here), which is what `dns-sd` and iOS `NWBrowser` query; the `mdns-sd`
crate runs its **own** responder and its records never reach Apple's. The hub's
advert works only because the hub runs on **Linux**, where `mdns-sd` owns the
mDNS stack. Confirmed: `dns-sd -B _audioshare._tcp` lists the (Linux) hub but
`dns-sd -B _audioshare-dongle._tcp` lists nothing while the Mac agent runs.

This is a **macOS-as-dongle dev artifact, not a protocol/app bug** — production
dongles are Linux SBCs where discovery works (same as the hub). Two ways to test
the iOS sub-step 2.5 path regardless:
- **Real verification:** run the agent on a Pi/Linux box; the iPhone discovers it
  natively, no shim.
- **Mac dev shim:** advertise the service through Apple's responder while the
  *real* agent still handles the TCP assignment on 50507 —
  `dns-sd -R "<name>" _audioshare-dongle._tcp local 50507 id=<agent's real uuid> name="<name>"`
  (keep it running). The app then discovers via Apple's advert, resolves the
  TXT `id`/`name` + the Mac's IP, and connects to 50507 (the agent), which
  receives `Assign`, persists, acks, and registers with the hub. The `id` TXT
  **must** be the agent's persisted UUID so the hub registers the output/zone
  under the id playback will target.

(If macOS dongle discovery ever needs to work natively, the agent would have to
advertise via Apple's `dns_sd` C API on macOS instead of `mdns-sd` — not worth
it for a Linux-only production target.)

#### Sub-step 3 — landed (2026-06-21)

Per-zone independent Snapcast streams + hub-driven grouping. Builds directly on the sub-step 2 architecture; no protocol or crate layout changes. Update `CLAUDE.md` (protocol/state source of truth) when this section changes.

**What it delivers.** Each dongle zone plays its own independent audio stream; a user-created multi-dongle zone plays one synchronized stream across all members (Snapcast clock-aligns them). Zone CRUD (create/rename/delete zones, set membership) is live both in the engine and over the wire. The manual `Group.SetStream` bring-up workaround from note #2 is gone — the reconciler handles it automatically.

**`SnapcastSink` backpressure fix (sub-step 3.1, `audio/snapcast.rs`).** Prior to this step the sink opened the FIFO non-blocking and dropped samples when the pipe was full, causing choppy audio (KAN-23 Snapcast variant). Fix: keep the blocking open (already landed as a cold-start fix in sub-step 2), and leave the file descriptor blocking thereafter so subsequent writes naturally pace the decode thread instead of dropping overflow. One `SnapcastSink` instance per pool FIFO (no longer a single shared sink).

**Multi-stream pool (sub-step 3.2, `audio/snapcast.rs`).** `SnapserverSupervisor` now launches `snapserver` with `STREAM_POOL_SIZE = 16` pipe streams named `as-0` … `as-15`, each reading its own FIFO at `/tmp/audioshare-snapfifo-{k}`. A `StreamPool` tracks which slot is allocated to which zone. Pool is sized for concurrent *playing* zones, not total dongles — creating zones is unbounded; playing zones consume a slot.

**JSON-RPC control client (sub-step 3.3, `audio/snapcast_control.rs`).** `CommandConn` maintains a persistent TCP connection to `snapserver`'s control port (`127.0.0.1:1705`), speaking newline-delimited JSON-RPC 2.0. Requests used: `Server.GetStatus`, `Group.SetClients`, `Group.SetStream`. `EventListener` reads `snapserver` push notifications (`Client.OnConnect`, `Client.OnDisconnect`, `Server.OnUpdate`) on the same socket and signals the reconciler. Both are defined behind a `SnapcastControl` trait for unit testing against a mock.

**`SnapcastRouter` + desired-state reconciler (sub-step 3.4, `audio/snapcast_router.rs`).** The `SnapcastRouter` is the engine's single seam into Snapcast — the engine never speaks JSON-RPC itself. It owns the `SnapserverSupervisor`, the `SnapcastControl` client, and the stream pool. API:
- `sink_for_zone(zone, dongle_ids)` — allocates a free pool slot (`no_free_stream` if all 16 are in use), records desired grouping (zone → stream, dongle → zone), fires a reconcile, returns the slot's `SnapcastSink` to the decode thread.
- `release_zone(zone)` — frees the pool slot; clients stay grouped on the now-idle (silent) FIFO to avoid churn.
- `reconcile_now()` (idempotent) — for each active zone: `GetStatus` → find the zone's dongles' clients → `Group.SetClients(group, client_ids)` → `Group.SetStream(group, slot_stream_id)`. Also fired on every `Client.OnConnect`/`OnDisconnect` notification, so a late-connecting `snapclient` is pulled onto the right stream automatically.

**Engine changes (sub-step 3.4, `audio/engine.rs`).** The shared `Arc<SnapcastSink>` field is replaced by a `SnapcastRouter`. Dongle `Output`s carry `sink: None` — `Output.sink` is now `Option<Arc<dyn AudioSink>>`; only the local cpal output carries a real sink. The router allocates a per-zone FIFO sink at play time. Zone routing in `play`: all-local → existing single/FanOut local path (unchanged); any-dongle (enforced all-dongle) → `router.sink_for_zone(...)`. Mixed local+dongle → blocked at `set_zone_outputs` time (`mixed_zone_unsupported`) and defended inside `zone_sink` as well.

**Zone CRUD + wire protocol (sub-step 3.5).** New engine methods: `create_zone(name) -> ZoneId` (generates a UUID id, empty membership; duplicate names allowed), `delete_zone(zone)`, `rename_zone(zone, name)`, `set_zone_outputs(zone, [output_ids])` (enforces all-dongle or all-local). All four are exposed as wire tasks routed by `commands::dispatch()`. New wire push: `zones` message on every `OUTPUTS_CHANGED` trigger and on `list_zones` pull — carries `{ zone, name, outputs, playing }` per zone (full detail in `CLAUDE.md` protocol section). The flat `outputs` push is retained for iOS back-compat.

**Device-free tests cover:** stream pool allocate/release/exhaustion; reconciler logic against a mock `SnapcastControl`; `SnapcastControl` JSON-RPC request/response + notification parsing against a loopback mock snapserver; engine zone CRUD + constraint enforcement; `SnapcastSink` backpressure via a real temp FIFO + draining reader thread. Demo-gated path (needs `snapserver` + 2× `snapclient` + audio hardware): two dongles → independent audio; `create_zone` + `set_zone_outputs` grouping both → synchronized audio; late-joining dongle auto-assigned to the correct stream.

**Still deferred:** iOS grouping UI (own spec; wire contract frozen in `CLAUDE.md`); auth on the dongle registration/assignment channels; general local-output jitter buffer (KAN-23 base, not the Snapcast variant).

#### Bring-up notes (first real laptop-as-dongle demo, 2026-06-20)

The sub-step 2.3 path was proven end-to-end (Pi hub → laptop running the agent +
stock `snapclient` → audio out the laptop). Hard-won gotchas, so the next session
doesn't rediscover them:

1. **Disable the distro's `snapserver.service`.** `apt install snapserver`
   enables a systemd service that auto-starts a `snapserver` on boot, which grabs
   ports **1704/1705**. The hub spawns and supervises its **own** `snapserver`
   (`SnapserverSupervisor`), so the system one squats on the port and the hub's
   instance can't bind it — `snapclient` then connects to the *system* server's
   empty `default` stream and you get silence. Fix on the Pi:
   `sudo systemctl disable --now snapserver`. **This belongs in the install docs /
   flashable-image setup** — it'll bite every user. (A more self-contained fix:
   have the hub launch `snapserver` with an explicit minimal config so it never
   collides — worth considering for packaging.)
2. ~~**New `snapclient`s land on the wrong stream.**~~ **Retired — automated by sub-step 3 reconciler.** Previously, `snapserver.conf`'s `default` stream trapped freshly-connected clients, requiring a manual `Group.SetStream {id:<group>, stream_id:"AudioShare"}` via the control RPC on 1705. The sub-step 3 desired-state reconciler now handles this automatically: every `Client.OnConnect` notification triggers a reconcile that calls `Group.SetClients` + `Group.SetStream` to pull the client onto the correct pool stream. No manual intervention needed; the workaround is not persistent across reconnects and is gone.
3. **iOS hardcoded `zone:"default"`.** `Library.swift` sent every `play`/`stop`
   with `zone:"default"`, so playback always hit the hub's local output, never a
   dongle. Added a free-text **Zone** field (type a dongle UUID to target it) as a
   stepping stone toward the 2.5 picker. The dongle's UUID (its `OutputId`) is
   printed at agent startup and is the zone id to target.
4. **Choppy audio over the Snapcast path (KAN-23, Snapcast variant).** Real
   playback came through but stuttered ("stop and start"). Root cause is the
   `SnapcastSink`: it opens the FIFO **non-blocking** and **drops** samples on
   `WouldBlock` (pipe full). That non-blocking stance is right *before* snapserver
   is reading (so the decode thread never stalls waiting for it to come up), but
   once snapserver is draining the FIFO at real time a **blocking** write is the
   natural backpressure that paces the decode thread — dropping on overflow
   instead guarantees gaps. Fix direction: keep the non-blocking lazy open to
   detect snapserver, then switch the handle to blocking once connected (clear
   `O_NONBLOCK` via `fcntl`), or feed snapserver through a paced ring buffer. This
   is the Snapcast-path sibling of the local-output prebuffering already done in
   commit 550a50d; the local fix doesn't cover this sink.

#### Bring-up notes (Pi-hub → Pi-dongle, cold-start race fixed, 2026-06-21)

Full path proven again (macOS hub → Pi dongle agent → `snapclient` → speaker).
Registration was clean once both ends ran current binaries; the only hard failure
was the **first play producing no sound**.

5. **Cold-start race — fixed (commit 9c70373).** `SnapcastSink::open_fifo_write`
   opened the FIFO `O_NONBLOCK`, which returns `ENXIO` whenever `snapserver`'s read
   end is momentarily closed (it cycles open/closed while it has no writer). The
   non-blocking writer and the polling reader missed each other and **every decoded
   buffer was silently dropped** — measured 17s–2min before audio, past
   `snapclient`'s 5s no-chunk timeout — so the first play after a fresh client
   connection was silent until the race happened to resolve. Fix: open the FIFO
   **blocking**; it returns the instant `snapserver` next opens the read end
   (~80ms) and we then hold a persistent writer. (This supersedes the
   "non-blocking open then `clear_nonblocking`" direction floated in note 4 above —
   blocking from the open is simpler and removes the race.)

**Open follow-ups (need to be taken care of — not yet done):**

- **`snapclient` binds ALSA `sysdefault` despite the `-s default` arg.** The agent's
  `SnapclientSupervisor` passes `-s default`, but `snapclient` still logs
  `device: sysdefault`. It worked on the test Pi, but `sysdefault` is HDMI-only
  card 0 on many Pis (ALSA error 524 with nothing plugged into HDMI), so this will
  bite on other hardware. Verify the `-s` arg actually reaches `snapclient` and
  resolves to the intended sink (`AUDIOSHARE_SOUND_DEVICE` override exists).
- **Stale `connected:false` `snapclient` entries accumulate in `snapserver`.** Each
  dongle reconnect (and any hostID change) leaves a ghost client in
  `Server.GetStatus`; the list grows across runs. Cosmetic today, but should be
  reaped / deduped before the iOS picker surfaces snapserver state directly.

---

## Suggested execution order

1. **Change 1** (`AudioSink` trait) — pure refactor, no protocol change, lowest
   risk. Honest first commit.
2. **Changes 2 + 3** (registry + zone engine) with only the local output and a
   `"default"` zone — verify zero behavior regression.
3. **Change 4** (zone in protocol) + iOS mirror + `CLAUDE.md` update.
4. **Change 5** — first real second output: `SnapcastSink` + snapserver
   supervision, then a **custom dongle agent that wraps Snapcast** (workspace
   crate in `audio_share`) + registration, then hub-driven grouping, then the
   flashable image (separate repo). See Change 5's own sub-step ordering — start
   with stock `snapclient` to prove the path before building the custom agent.

This never blocks progress on dongle hardware, and the risky network/Snapcast
work is last, behind a stable interface.

---

## Verification

- **Per step, run existing tests:** `cargo test` (unit tests in
  `output.rs`/`decode.rs`/`commands.rs` are device-free and must stay green).
- **End-to-end audio (opt-in, needs audio hardware + network):**
  `cargo test audio::decode::tests::plays_internet_radio_briefly -- --ignored --nocapture`
  — you should hear ~3s of SomaFM. After Changes 1-3, this must still play
  (the local device is now reached via the trait/registry/default zone).
- **macOS caveat:** the binary reads `/proc/cpuinfo` for the Pi serial and exits
  on macOS; stub `get_serial_number()` or test the audio engine in isolation via
  the above `cargo test` paths (they don't require the full server boot).
- **Change 4 manual check:** send a `play` task with `data.zone` set and confirm
  the response echoes `ok`; send an unknown zone and confirm `unknown_zone`.
- **Change 5:** with one dongle flashed, confirm it appears in the registry on
  connect, plays when its zone is targeted, and a grouped zone stays in sync
  (Snapcast).

---

## Roadmap mapping (for cross-reference)

- Change 1-4 ≈ **Phase 2 — Independent multi-room** (the headline feature).
- Change 5 (Snapcast path) ≈ **Phase 3 — Synchronized multi-room**.
- Phase 4 (AirPlay/Spotify receiver) and Phase 5 (more DRM-free sources) sit on
  top of this and are out of scope here.
- Jira project **KAN** will be reconciled with these phase numbers later.
