# AirPlay Slice 4 — Reroute — Design

> Hub-side design for the final AirPlay receiver slice: moving a **live** AirPlay
> session from one zone to another while the phone stays connected to the same
> receiver. A small, well-bounded delta on
> `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md` (the parent
> AirPlay design) — read that first for the source/zone model. Cross-check against
> `CLAUDE.md` (protocol/state source of truth) and `docs/multi-room-plan.md`
> (engine architecture); freeze the wire contract into `CLAUDE.md` when this lands.
> The iOS reroute UI is a **separate follow-on spec**, out of scope here.

---

## Context & goal

Per the parent AirPlay design, receivers are named **sources** decoupled from
destinations: each zone appears in the iPhone's AirPlay menu named after the zone,
and the engine maps each active source to a `dest_zone`. Slices 1–3 shipped the
receiver path, per-zone receivers + default source→zone routing, and track
metadata + album art. The one remaining operation is **reroute**: redirect a live
session's audio to a different zone internally, with the phone none the wiser
(it stays connected to the same `shairport-sync` receiver).

**Demo (demo-gated, not CI):** audio playing to Kitchen; tap in the app → it moves
to Living Room with the phone still connected to the Kitchen receiver.

This is the last slice of Phase 4's AirPlay work. After it, `CLAUDE.md`'s "Reroute
(Slice 4) is not yet built" notes are removed.

## What's already in place

Slices 2–3 left almost all of the scaffolding. Reroute composes existing pieces;
it does **not** introduce a new subsystem.

- `AirplaySource` already carries a mutable `dest_zone`, a `routed` flag
  (`false` = connected-but-unrouted), and a per-session cached `sink:
  Option<Arc<dyn AudioSink>>`. The source's map key / `id` **is** its home zone id.
- `detach_driver(ZoneDriver)` already shuts down a URL pipeline or marks an
  AirPlay source unrouted (`routed = false`, `sink = None`).
- `session_began` already installs a source as a zone's driver with last-wins
  over whatever drove that zone.
- `sink_for_source` already resolves a zone's sink lazily via `zone_sink()` and
  caches it on the source.
- The PCM pump (`airplay::run_receiver` → `pump_open`) calls its `resolve()`
  closure (`sink_for_source`) **per chunk** (every ~8 KB FIFO read) and rebuilds
  the resample pipeline when the resolved sink's `(sample_rate, channels)`
  changes. **Reroute therefore needs no thread restart** — invalidating the
  cached sink makes the next chunk re-resolve to the new zone and rebuild the
  resampler if needed. (This supersedes the parent design's "stop the reader,
  restart it" sketch, which predated the pull-driven pump.)

## Engine operation: `reroute`

New public method on `Engine`:

```rust
pub fn reroute(&self, source: &str, new_zone: &str) -> Result<(), RerouteError>;

enum RerouteError {
    UnknownSource,        // -> "unknown_source"
    UnknownZone,          // -> "unknown_zone"
    ZoneHasNoOutputs,     // -> "zone_has_no_outputs"
    MixedZoneUnsupported, // -> "mixed_zone_unsupported"
    NoFreeStream,         // -> "no_free_stream"
}
```

Steps (mirroring the existing lock discipline — snapshot under a lock, release
before any blocking I/O, reacquire to install):

1. **Validate the source** under the `sources` lock: it must exist **and** be
   `active`, else `UnknownSource`. Reroute is permitted on a connected-but-unrouted
   source (`active && !routed`) — that is exactly how the user rescues a source
   whose zone got taken over, by moving it to a free zone. Snapshot the current
   `dest_zone` while holding the lock.
2. **No-op fast path:** if `new_zone == dest_zone`, return `Ok(())` without firing
   pushes or touching topology.
3. **Validate `new_zone` exists** (under the `zones` lock, snapshotting its
   `outputs`), else `UnknownZone`.
4. **Eager sink resolution** off the locks: call `zone_sink(new_zone, outputs)`.
   This matches `play`'s behavior and is the source of the remaining error codes —
   `ZoneHasNoOutputs`, `MixedZoneUnsupported`, `NoFreeStream` surface here. **On
   any failure the source is left entirely unchanged** (still routed to its old
   zone, still playing); reroute is atomic from the user's perspective.
5. **Commit** (reacquire locks):
   - Detach the **old** dest zone's driver: if that zone's `current` is
     `Airplay(source)`, clear it and `release_zone(old_dest)` to free any Snapcast
     pool slot the old zone held.
   - Detach whatever currently drives **new_zone** via `detach_driver` (last-wins:
     a URL pipeline is shut down; another AirPlay source is marked unrouted).
   - On the source: set `dest_zone = new_zone`, `routed = true`, and cache the
     `sink` resolved in step 4.
   - Install `ZoneDriver::Airplay(source)` on `new_zone`.
6. Fire `SOURCES_CHANGED` (source's `dest_zone` changed) and `OUTPUTS_CHANGED`
   (both zones' `playing` state changed).

The live pump picks up the newly cached sink on its next per-chunk `resolve()`;
no reader thread is stopped or restarted. If the new zone's sink has a different
format than the old one, the pump rebuilds its resampler automatically.

## Revert-to-home on session end

Reroute is a **per-session, live override**, not a persistent reassignment. The
iOS AirPlay target's name stays the source of truth for where that receiver plays
by default.

`session_ended` gains one change: reset `dest_zone` back to the source's home zone
(`s.dest_zone = source.to_string()`, since the id == home zone id) alongside the
existing `active = false` / `routed = false` / `sink = None` / metadata clears. A
future session on that receiver thus resumes at its home zone; the prior reroute
does not leak across sessions.

(`session_began` already reads `s.dest_zone` to choose its destination, so no
change is needed there — after revert it naturally reads the home zone.)

## Receiver name during reroute (constraint, not a choice)

Reroute mutates only the internal `dest_zone`; it **never** changes the receiver's
advertised Bonjour/mDNS name. After a Kitchen → Office reroute, the phone stays
connected to the **Kitchen** receiver and its native AirPlay menu keeps showing
**"Kitchen"**, while audio plays out of the **Office** zone.

This name/destination mismatch is intentional and is also a hard constraint of
classic AirPlay: a session is bound to a fixed Bonjour service name, so renaming a
live receiver requires restarting its `shairport-sync` instance, which tears down
the RTSP session — i.e. it **disconnects the phone**. That would defeat reroute's
whole promise (the phone stays connected while we redirect internally). The only
time a receiver's name changes is `rename_zone` (Slice 2), which already restarts
it. Consequently:

- **iPhone AirPlay name = which receiver (source) the user picked** — "what am I
  streaming *from*."
- **Our app = where it's actually playing (`dest_zone`)** — "where is the sound
  *now*." The `sources` push carries `dest_zone` precisely so the app can render
  "Kitchen receiver → playing in Office"; the iPhone menu cannot convey this once
  rerouted. This is why the now-playing/reroute view lives in our app, per locked
  decision #2 (sources decoupled from destinations).

## Wire protocol additions

Uses the existing encrypted + newline-framed channel and the standard
`{ status, task, data?, error? }` envelope. Frozen into `CLAUDE.md` when this
lands (the parent design already pre-registered these shapes).

**New task — `reroute` (iOS → server), routed through `commands::dispatch()`:**

```json
// request
{ "task": "reroute",
  "data": { "source": "<home-zone-id>", "zone": "<dest-zone-id>" },
  "session_token": "..." }
// success
{ "status": "ok", "task": "reroute" }
```

`source` is the receiver's stable handle (its home zone id, as carried in the
`sources` push); `zone` is the new destination. `dispatch()` reads both fields,
calls `ENGINE.reroute(source, zone)`, and maps `RerouteError` to error codes.

**Error codes:**

- **New:** `unknown_source` — `reroute` referenced a source/receiver the engine
  doesn't know (or one with no active session). (Also reserved for `get_art`'s
  unknown-source case, harmonizing with the parent design.)
- **Missing-field guards** — following `play`'s per-field precedent (`missing_url`
  for a `play` with no `data.url`): a `reroute` with no `data.source` → New code
  `missing_source`; with no `data.zone` → New code `missing_zone`. Checked in
  `dispatch()` before calling the engine.
- **Reused:** `unknown_zone`, `zone_has_no_outputs`, `mixed_zone_unsupported`,
  `no_free_stream` — all surface from the eager `zone_sink()` resolution, same as
  `play`.

No new push message: reroute reuses the existing `sources` push (it fires
`SOURCES_CHANGED`, carrying the updated `dest_zone`) and the `outputs`/`zones`
pushes (via `OUTPUTS_CHANGED`).

## Testing & verification

**Device-free unit tests (CI), mirroring existing engine/dispatch tests with a
dummy `AudioSink` and fake sources (no real `shairport-sync`):**

- Reroute an active source from its home zone to a new zone → `dest_zone` moves,
  old zone's `current` cleared, new zone's `current` is `Airplay(source)`.
- Last-wins on the target: reroute onto a zone playing a URL shuts the URL
  pipeline down; reroute onto a zone fed by another AirPlay source marks that
  other source unrouted.
- Reroute to the **same** zone is a no-op (no topology change, no push).
- Reroute a **connected-but-unrouted** source (`active && !routed`) onto a free
  zone routes it (`routed` becomes true).
- `unknown_source` (no such source / source not active); `unknown_zone`.
- Reroute onto a zone with no outputs returns `zone_has_no_outputs` **and leaves
  the source on its old zone, still routed** (atomic-failure guarantee).
- `session_ended` after a reroute resets `dest_zone` to the home zone; a
  subsequent `session_began` routes to the home zone.
- Dispatch-level mapping: each `RerouteError` variant → its error code; missing
  `source`/`zone` fields → their guard codes.

**Demo-gated (needs `shairport-sync` + a real AirPlay sender + audio hardware),
not CI:** the end-to-end "audio follows the tap, phone stays connected" demo.

## Out of scope (V1)

- iOS reroute UI (separate follow-on spec). This slice ships hub-side logic + the
  frozen wire contract only.
- Persistent / sticky reroute across sessions (explicitly rejected — see
  revert-to-home above).
- Transport/volume control back to the sender (unchanged from the parent design).

## Docs to update when this lands

- `CLAUDE.md`: remove the "Reroute (Slice 4) is not yet built" notes (AirPlay
  Slice 3 paragraph and elsewhere); add `reroute` to the recognized-tasks list and
  the error taxonomy (`unknown_source`, any missing-field guards); note Slice 4
  done.
- `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md`: mark Slice 4
  `[DONE]` with an as-built note (notably: reroute is cache-invalidation on the
  pull-driven pump, **not** a reader restart; revert-to-home on session end).
