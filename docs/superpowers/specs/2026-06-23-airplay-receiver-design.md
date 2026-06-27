# AirPlay Receiver (Phase 4) — Design

> Hub-side design for receiving audio pushed from the phone's own apps via
> AirPlay. This is the first **receiver** play-mode: audio is pushed *to* us, not
> fetched by us. Cross-check against `CLAUDE.md` (protocol/state source of truth)
> and `docs/multi-room-plan.md` (engine architecture) — update both as slices
> land. iOS now-playing/reroute UI is a **separate follow-on spec**.

---

## Context & goals

Per the 2026-06 pivot, Audio Share is legally *a speaker*: audio reaches a
speaker either because the hub plays a DRM-free source itself (done — internet
radio, the URL path) or because a **receiver protocol lets the phone's own app
push audio to us** (this spec). AirPlay receive has **zero licensing exposure** —
the user authenticates Spotify/Apple Music/etc. on their own phone and pushes to
the hub. This is roadmap **Phase 4**, built on top of the multi-room engine
(Changes 1–5).

**What V1 delivers:** every Audio Share zone appears as its own AirPlay target in
the iPhone's native AirPlay menu, named after the zone. The user picks a zone
there and audio plays to it (the hub's local output *or* a dongle group, reusing
Snapcast unchanged). In our app the user can **see what's playing where** (track
title/artist/album + album art, and which zone) and **reroute** a live session to
a different zone — the phone stays connected to the same receiver while we
redirect its audio internally.

### Locked decisions

These were settled during brainstorming; the design assumes them.

1. **Receiver topology: one receiver per zone**, named after the zone.
2. **Receivers are named *sources*, decoupled from destinations.** The app maps
   each active source to a destination zone and can **reroute** it
   (source→zone). "Kitchen" receiver → Kitchen zone is just the default.
3. **Now-playing detail: track metadata** — title/artist/album + album art.
4. **Scope: hub-side + wire contract only.** Freeze the wire protocol in
   `CLAUDE.md`; the iOS now-playing/reroute UI is its own follow-on spec (same as
   the zone-grouping UI was handled).
5. **AirPlay flavor: classic AirPlay (AirPlay 1) for V1.** `shairport-sync`
   cannot run multiple **AirPlay 2** instances on one host (one IP) — AirPlay 2
   clients are confused by several AP2 players at the same IP, and current NQPTP
   serves only one instance. Multiple **classic** instances on one host are well
   supported (distinct device id + output each). Classic still works from any
   modern iPhone/Mac; audio is identical ALAC 44.1/16; the cost is classic's
   realtime (~2 s) buffer vs AP2's deep buffering, and no Apple-side multi-room
   (we don't need it — Snapcast does our grouping).
6. **Endgame is hub-side AirPlay 2 (path "3a"):** reach per-zone AP2 later by
   giving each instance its own IP (macvlan / virtual interfaces). This keeps the
   hub-centric source→zone routing + reroute + Snapcast transport that V1 builds.
   The classic→AP2 swap is then mostly (a) flip each instance to AP2 + NQPTP and
   (b) per-instance IP — the latter is packaging/infra, not app logic. So ~90% of
   V1 is reused. (The alternative "3b" — an AP2 receiver on each *dongle* —
   bypasses the hub and abandons reroute; it was explicitly **not** chosen.)

### Out of scope (V1)

- iOS UI (separate spec).
- AirPlay 2 (deferred to 3a) and per-instance IP networking.
- Transport/volume control *back* to the sender (would want shairport's D-Bus
  interface; see alternatives). V1 surfaces volume state at most, doesn't drive
  it.
- Spotify Connect / Chromecast receivers (separate, optional-plugin work).

---

## Integration approach

**Chosen: `shairport-sync` as a supervised "source" feeding the existing
`AudioSink` seam.** Each receiver is a `shairport-sync` process (classic AirPlay)
configured with two outputs:

- the **`pipe` audio backend** → a named FIFO of raw PCM (`S16LE` 44100/2,
  AirPlay's fixed format) that a reader thread converts to `f32` and writes into
  a destination zone's `AudioSink` — the same shape as
  `decode::stream_url_to_output` for URLs, and the inverse of the `f32`↔`s16le`
  conversion `SnapcastSink` already does;
- the **`metadata` pipe backend** → a second FIFO carrying shairport's DAAP/DACP
  metadata stream (session begin/end, track title/artist/album, `PICT` album
  art) that a metadata reader parses.

This leaves decode, the registry, `zone_sink()`, and Snapcast grouping
**unchanged** — an AirPlay source resolves to a zone's sink through the same
`zone_sink()` path a URL does, so a dongle zone "just works" (sink = a snapserver
FIFO) and reroute = swap which zone's sink the reader writes to.

**Alternatives considered (rejected):**

- **snapserver's built-in `airplay` stream source** (snapserver owns
  `shairport-sync`) — only covers dongle/snapserver zones (not the local cpal
  output), moves routing *into* snapserver (fights "the hub zone model is the
  source of truth"), and doesn't fit source→zone reroute.
- **shairport-sync D-Bus/MPRIS control** instead of the metadata pipe — richer
  structured control (could give us transport/volume back to the sender later),
  but adds a D-Bus dependency and fiddlier album-art handling. The metadata pipe
  is the simpler, well-trodden path (Volumio/moOde use it). A possible later
  upgrade, not V1.

---

## Engine model — the "source" abstraction + reroute

The engine gains a **sources** registry alongside zones. A *source* produces PCM;
a *zone* consumes it. Today a zone's `current` is implicitly "a URL pipeline or
nothing"; we generalize so a zone can be driven by either a URL or an AirPlay
source, and enforce **one driver per zone**.

**New state on `Engine`:**

```rust
sources: Mutex<HashMap<SourceId, AirplaySource>>

struct AirplaySource {
    id: SourceId,                 // == home_zone id; stable handle for the receiver
    name: String,                 // AirPlay name shown in iOS (defaults to home zone's name)
    home_zone: ZoneId,            // default destination
    dest_zone: ZoneId,            // current routing target; reroute mutates this
    supervisor: ShairportSupervisor,   // the shairport-sync process
    session: SessionState,        // Idle | Active { metadata, client_name }
    reader: Option<ReaderHandle>, // the PCM-pump thread; present only when routed + active
}
```

**Generalize the zone's driver** so last-wins lives in one place:

```rust
enum ZoneDriver { Url(Pipeline), Airplay(SourceId) }
// ZonePlayback.current: Option<ZoneDriver>   (was Option<Pipeline>)
```

**Operations:**

1. **Session begins** (metadata reader sees AirPlay play-begin `pbeg`): resolve
   `dest_zone` → sink via the existing `zone_sink()`; detach whatever currently
   drives that zone (shut down a `Url` pipeline, or detach another source's
   reader — last-wins); spawn a reader thread (PCM FIFO → `f32` → `sink.write()`);
   set the zone's driver to `Airplay(id)`; fire pushes.
2. **Reroute** (`reroute(source_id, new_zone)`): stop the source's reader,
   resolve `new_zone`'s sink (last-wins on the new zone too), restart the reader
   against it, update `dest_zone`, fire pushes. The phone stays connected to the
   same receiver throughout.
3. **Session ends** (play-end `pend` / disconnect): stop the reader, clear the
   zone's driver if it was this source, mark `Idle`, fire pushes.

**Conflict / edge rule (V1, documented):** a zone has at most one driver. If
something takes over a zone an AirPlay source was feeding, that source goes
**connected-but-unrouted** — its PCM is drained/discarded until rerouted to a
free zone or the session ends. Symmetrically, starting AirPlay on a zone playing
a URL shuts the URL pipeline down. This reuses `play`/`stop`'s existing per-zone
shutdown logic; the URL and Snapcast paths are untouched.

**Lock discipline:** mirror the existing engine — snapshot under the `zones`
lock, release it before any I/O that can block (spawning a process, a snapserver
JSON-RPC round-trip via `zone_sink`), then reacquire to install state. `sources`
is an independent mutex.

---

## shairport-sync supervision & lifecycle

**One receiver per zone, tied to zone lifecycle.** A `ShairportSupervisor`
mirrors `SnapserverSupervisor`/`SnapclientSupervisor`
(spawn → monitor → restart-on-exit → kill-on-drop). A `ShairportManager` owns a
`HashMap<ZoneId, ShairportSupervisor>` and **reconciles against the zone set** at
the points zone topology already changes:

| Trigger | Action |
|---|---|
| engine start / AirPlay enabled | spawn a receiver for each existing zone |
| `create_zone` / dongle auto-zone created | spawn a receiver named after the zone |
| `delete_zone` | kill its receiver |
| `rename_zone` | restart the receiver with the new mDNS name |
| `set_zone_outputs` | **no receiver change** — routing resolved per-session via `zone_sink()` |

Every zone (the Hub/default zone, each dongle's auto-zone, each user-created
group) thus appears as its own classic-AirPlay target named after the zone.

**Per-instance identity (classic multi-instance requirements):**

- **Name** = zone display name; advertises `_raop._tcp` via the **system avahi**
  (shairport's avahi backend — independent of, and avoiding, the hub's
  `mdns-sd`/avahi conflict recorded in project memory).
- **Unique RTSP port** from a base (`5000 + slot`) and a **unique
  `airplay_device_id`** per instance — both required so AirPlay 1 clients don't
  conflate instances.
- **Two FIFOs** per instance: `/tmp/audioshare-airplay-{slot}.pcm` (audio) and
  `…-{slot}.meta` (metadata), via a generated minimal config / CLI args. (Slot
  index, not raw zone id, keeps paths/ports stable and filesystem-safe.)
- **No NQPTP** — that is an AirPlay 2 dependency; classic V1 doesn't need it (it
  returns at 3a).

**Per-instance threads:** the **metadata reader runs continuously** while the
instance is up (how we learn a session began — `pbeg`/`pend` bracket the session
and carry track info); the **PCM reader thread starts on `pbeg`** and stops on
`pend`.

**Resource note / default:** classic `shairport-sync` is light when idle (a few
MB, ~no CPU with no session), so a handful of zones on a Pi is fine. Default is a
receiver for *every* zone; a per-zone opt-in flag or soft cap is a later
refinement, not V1.

---

## Wire protocol additions

All new messages use the existing encrypted + newline-framed channel and the
standard `{ status, task, data?, error? }` envelope. A receiver's `source` id is
its **home zone's id** (the stable handle for that receiver); `dest_zone` is what
moves when you reroute.

**New push — `sources` (server → iOS).** Fired on a new `SOURCES_CHANGED`
broadcast (session begin/end, metadata update, reroute), same
subscribe-and-repush pattern as `outputs`/`zones`. Carries **only currently
active** AirPlay sessions (an idle receiver is just a zone with no session, which
the client already has from `zones`). Pullable via `list_sources`:

```json
{ "status": "ok", "task": "sources",
  "data": { "sources": [
    { "source": "<home-zone-id>", "name": "Kitchen",
      "dest_zone": "<zone-id>", "active": true,
      "client": "Chris's iPhone",
      "title": "Song", "artist": "Artist", "album": "Album",
      "art_id": "<hash-or-empty>" } ] } }
```

**Album art is fetched by reference, not inlined** (art can be hundreds of KB;
inlining would bloat every metadata tick). The push carries `art_id` (a hash of
the current image bytes; empty if none); the client fetches once per change via a
**`get_art` task**:

```json
// request
{ "task": "get_art", "data": { "art_id": "<hash>" }, "session_token": "..." }
// response
{ "status": "ok", "task": "get_art",
  "data": { "art_id": "<hash>", "mime": "image/jpeg", "image": "<base64>" } }
```

The hub caches the latest art per source keyed by hash.

**New task — `reroute` (iOS → server):**

```json
{ "task": "reroute", "data": { "source": "<home-zone-id>", "zone": "<dest-zone-id>" },
  "session_token": "..." }
// success: { "status": "ok", "task": "reroute" }
```

**Task routing:** `reroute` and `get_art` route through `commands::dispatch()`;
`list_sources` is special-cased in `handle_task` (like `list_outputs`/
`list_zones`) because it pushes.

**New error codes:**

- `unknown_source` — `reroute`/`get_art` referenced a source/receiver the engine
  doesn't know.
- `unknown_art` — `get_art` with an `art_id` not in the cache (art changed
  underneath).
- `reroute` also reuses existing `unknown_zone`, `zone_has_no_outputs`,
  `mixed_zone_unsupported`, `no_free_stream` (it resolves a sink through the same
  `zone_sink()` path).

This is frozen into `CLAUDE.md`'s protocol section as slices 2–4 land.

---

## Slicing (vertical sub-steps)

Each slice ends at something demoable; the most-additive UX lands last.

- **Slice 1 — Prove the receiver path to the hub's local output.** One classic
  `shairport-sync` (the default/Hub zone's receiver) → audio FIFO → PCM reader
  (`s16le`→`f32`) → the local `AudioSink`. No metadata, no per-zone, no reroute.
  Lands `ShairportSupervisor` + config/arg building, the PCM reader, one source
  wired into the engine. *Demo:* iPhone picks "Hub" in AirPlay, sound comes out
  the hub speaker. (Mirrors Change 5 sub-step 1 proving the path before custom
  code.)
- **Slice 2 — Per-zone receivers + default source→zone routing. [DONE]**
  `ShairportManager` reconciled against the zone set (one receiver per zone, incl.
  dongle auto-zones); session begin/end via the metadata pipe (`pbeg`/`pend`
  only); each source feeds its home zone through `zone_sink()` — so dongle zones
  route through the snapserver FIFO automatically. One-driver-per-zone conflict
  handling. Adds the `sources` push (active/`dest_zone`, no track info yet),
  `SOURCES_CHANGED`, `list_sources`. *Demo:* AirPlay to "Kitchen" → sound at the
  Kitchen dongle; the app shows Kitchen is receiving.

  > **As built — session-bracket deviation:** sessions are bracketed by the
  > **audio FIFO** (FIFO open = session began, EOF = session ended), not the
  > metadata pipe (`pbeg`/`pend`). The metadata pipe is introduced in Slice 3
  > (track info). Using the audio FIFO avoids interrupting a blocked PCM reader
  > and makes routing per-chunk (reroute-ready): each chunk resolves the current
  > sink through `sink_for_source()` so a reroute takes effect on the very next
  > write with no reader restart. This is a deliberate design choice, not a
  > shortcut, and it supersedes the metadata-pipe bracket described in the spec
  > body above for session lifetime purposes.
- **Slice 3 — Track metadata + album art.** Full metadata-pipe parse
  (title/artist/album, `PICT`), art cache keyed by hash, `get_art` task, `art_id`
  in the push. *Demo:* the app shows now-playing title/artist/album + art.

  > **As planned — Slice 3 decisions:**
  > - **Session lifecycle unchanged.** The audio FIFO still brackets the session
  >   (open = active, EOF = ended), per the Slice 2 deviation note above. The
  >   metadata pipe is consumed **only** for track info; it does not drive
  >   `pbeg`/`pend` session start/end. The metadata reader runs continuously while
  >   the instance is up (and survives a rename, since the FIFO persists — same as
  >   the audio pump).
  > - **`client` field is best-effort.** Parse the sender's device name when
  >   shairport surfaces it (e.g. `ssnc`/`snua` user-agent / DACP side); send an
  >   empty string when absent. Not a blocker for the slice.
  > - **Art cache = one latest image per source**, keyed by its content hash. A
  >   `get_art` with a stale `art_id` (art changed underneath) returns
  >   `unknown_art`. Bounds memory to ~one image per receiver.
  > - **`art_id` = SHA-256 hex** of the current image bytes (via the existing
  >   `sha2` crate; `base64` is likewise already a dependency — no new crates).
  > - **`session_ended` clears** the source's track metadata and cached art; every
  >   track/art change fires `SOURCES_CHANGED`.
- **Slice 4 — Reroute. [DONE]** The `reroute` task + engine reroute op + last-wins on
  the target zone. *Demo:* audio playing to Kitchen; tap in the app → it moves to
  Living Room with the phone still connected to the Kitchen receiver.
  > **As built — Slice 4:** reroute is **cache invalidation on the pull-driven
  > pump, not a reader restart** — the pump's per-chunk `sink_for_source` picks up
  > the rerouted zone's sink (rebuilding the resampler if the format differs).
  > Sink resolution is **eager** (errors surface in the `reroute` response; a
  > failure leaves the source on its old zone), and `session_ended` **reverts
  > `dest_zone` to the home zone** so a reroute does not persist across sessions.

`CLAUDE.md`'s protocol section is updated as slices 2–4 land.

---

## Testing & verification

**Device-free unit tests (CI), mirroring existing patterns:**

- `ShairportSupervisor` config/arg building (pure) — like the dongle agent's
  snapclient arg tests.
- **Metadata-pipe parser** fed recorded byte sequences → asserts parsed events
  (`pbeg`/`pend`, DAAP title/artist/album, `PICT` → art bytes/hash). Highest-value
  pure test.
- **Engine source model** — activate / reroute / one-driver-per-zone last-wins /
  conflict edge — using a dummy `AudioSink` and a fake source (no real
  `shairport-sync`), as zone CRUD is tested today.
- `ShairportManager` **reconciliation** (spawn/kill/rename diff vs the zone set)
  as a pure function — like the Snapcast reconciler.
- Art cache + `get_art` lookup (incl. `unknown_art`); `reroute` error codes
  (`unknown_source`, `unknown_zone`).
- A seam (a `Shairport` trait or supervisor-behind-trait) so engine/connection
  handling is testable without the process — same instinct as `SnapcastControl` /
  `DongleRegistrar`.

**Demo-gated (needs `shairport-sync` + a real AirPlay sender + audio hardware),
not CI:** the end-to-end demo for each slice above.

**Setup gotcha to document (install docs / flashable image):**
`apt install shairport-sync` enables a systemd `shairport-sync.service` that
auto-starts an instance grabbing an AirPlay name + ports — the direct analog of
the `snapserver.service` collision already documented in the multi-room plan.
Disable it (`sudo systemctl disable --now shairport-sync`) so the hub owns its
supervised instances.

**macOS caveat unchanged:** the binary reads `/proc/cpuinfo` and exits on macOS;
audio/engine tests run in isolation via `cargo test`, and the shairport path is
exercised on the Pi.

---

## Roadmap mapping

- This spec ≈ **Phase 4 — Receiver protocols** (AirPlay 2 receive), built on the
  multi-room engine (Phase 2/3, Changes 1–5).
- Endgame **3a** (hub-side AP2 via per-instance IPs) is a follow-on; V1's
  source→zone routing, reroute, supervision, metadata, and wire contract carry
  forward.
- iOS now-playing/reroute UI and Spotify Connect/Chromecast receivers are
  separate, later specs.
