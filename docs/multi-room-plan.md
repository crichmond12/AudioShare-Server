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
- **2.3** `crates/dongle_agent`: mDNS advertise + assignment listener + persisted
  hub address + register + **supervise `snapclient`** (reuse the
  `SnapserverSupervisor` spawn/monitor/restart/kill-on-drop pattern; the agent
  *delegates* — it never does transport or clock-sync itself). Demo gate: run the
  agent on a laptop, assign it (or `--hub`), `play {zone:<dongle>}` → sound.
- **2.4** Resilience (reconnect/backoff), optional heartbeat, docs (`CLAUDE.md`
  dongle protocol section + this doc), protocol round-trip + device-free
  registration tests.
- **2.5** iOS app: scan `_audioshare-dongle._tcp`, assign to the current hub —
  **cross-project**, mirrored in `~/Documents/Audio Share/`. Sequenced after the
  headless hub+agent path works.

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
