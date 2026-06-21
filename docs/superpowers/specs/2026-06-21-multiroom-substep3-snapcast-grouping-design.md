# Multi-Room Sub-step 3 — Hub-Driven Snapcast Streams & Grouping

> Design spec. Scope: **hub-side** implementation of independent per-dongle
> routing *and* synchronized grouping, driven by the hub's zone model over
> `snapserver`'s JSON-RPC control API. iOS grouping UI is **out of scope** (its
> own future spec); this spec freezes the wire contract that spec will mirror.
>
> Read alongside `docs/multi-room-plan.md` (Change 5, sub-step 3) and `CLAUDE.md`
> (protocol/state source of truth). Update both as commits land.

Date: 2026-06-21
Status: approved design, ready for implementation planning

---

## 1. Problem & goal

Today every registered dongle shares **one** `SnapcastSink` → **one**
`snapserver` stream, so routing audio to any dongle feeds *all* of them (one
synchronized group). This is the documented sub-step 2 limit.

Sub-step 3 makes zone membership real by having the hub **program `snapserver`
over its JSON-RPC API (port 1705)**:

- **Independent routing** — different dongles play different audio at once, each
  zone on its own stream.
- **Synchronized grouping** — several dongles bound to one stream play the same
  source, clock-aligned by Snapcast.
- **Zone CRUD** — create / rename / delete zones and set their membership, so a
  user can define multi-dongle groups on top of the per-dongle defaults.

Folded in: the **KAN-23 Snapcast-variant choppiness** fix (the `SnapcastSink`
drops samples on a full FIFO instead of applying backpressure), since multiple
streams make it worse and we are reworking that sink anyway.

### Non-goals

- iOS grouping UI (separate spec; wire contract is defined here and frozen).
- Auth on the dongle registration/assignment channels (still deferred).
- Mixing the hub's local output with dongles in one tight-synced zone (different
  sync domains — see §4 constraints).
- The general local-output jitter buffer (KAN-23 base; only the Snapcast sink is
  reworked here).

---

## 2. Guiding principle (unchanged from the plan)

**The hub's zone model is the single source of truth; the hub programs Snapcast
to match.** Sub-step 3 sharpens this into a **desired-state + reconciler**
design: the hub holds the intended Snapcast topology (which dongles belong to
which group, which group plays which stream) and a reconciler converges
`snapserver` to it on *any* trigger — zone change, playback change,
`snapserver`-reported client (dis)connect, or `snapserver` restart. Snapcast's
own streams/groups/clients model never becomes authoritative.

This is also what removes the bring-up race (a freshly-connected `snapclient`
landing on `snapserver.conf`'s `default` stream instead of ours): a client
connect notification triggers a reconcile that pulls it onto the right stream,
so the manual `Group.SetStream` workaround from the bring-up notes is gone.

Snapcast stays an implementation detail behind the `AudioSink` seam and the new
router — the engine never speaks JSON-RPC itself.

---

## 3. Components (all hub-side, in `audio_share`)

### 3.1 `SnapcastSink` — backpressure rework (`audio/snapcast.rs`)

- Keep the lazy, non-blocking open to **detect** that `snapserver` is reading the
  FIFO (avoids stalling the decode thread before `snapserver` is up).
- Once the FIFO opens successfully, **clear `O_NONBLOCK` via `fcntl`** so
  subsequent writes **block**, naturally pacing the decode thread instead of
  dropping overflow. This is the bring-up note #4 fix and the sibling of the
  local-output prebuffering in commit `550a50d`.
- One `SnapcastSink` instance per pool FIFO (no longer a single shared sink).

### 3.2 `SnapserverSupervisor` — multi-stream (`audio/snapcast.rs`)

- Launch `snapserver` with **N repeated `--stream.source` pipe streams**, named
  `as-0 .. as-(N-1)`, FIFOs `/tmp/audioshare-snapfifo-{k}`, each
  `mode=create&sampleformat=48000:16:2&codec=pcm`.
- Restart-on-exit and kill-on-drop behavior unchanged.
- `N` is a small constant (default **16**) — covers any home; sized for
  concurrent *playing* zones, not total dongles.

### 3.3 `SnapserverControl` — JSON-RPC client (new)

- Persistent TCP connection to `snapserver`'s control port (**1705**), speaking
  newline-delimited JSON-RPC 2.0.
- Requests used (the oldest, most stable RPCs only): `Server.GetStatus`,
  `Group.SetClients`, `Group.SetStream`.
- **Reads `snapserver` push notifications** on the same socket
  (`Client.OnConnect`, `Client.OnDisconnect`, `Server.OnUpdate`) and signals the
  reconciler.
- Reconnects if `snapserver` restarts; on reconnect the router replays the full
  desired state.
- Defined behind a trait so the reconciler can be unit-tested against a mock.

### 3.4 `SnapcastRouter` — the engine's one seam into Snapcast (new)

Owns the supervisor, the control client, the **stream pool**, and the
**desired-state reconciler**.

- **Stream pool:** N slots, each `{ stream_id, fifo_path, sink: Arc<SnapcastSink>,
  allocated_to: Option<ZoneId> }`.
- **Desired state:** `zone → stream_id`, `dongle_id → zone`.
- API to the engine:
  - `sink_for_zone(zone, dongle_ids) -> Result<Arc<dyn AudioSink>, String>` —
    allocate a free slot (→ `no_free_stream` if none), record desired grouping,
    trigger a reconcile, return the slot's sink.
  - `release_zone(zone)` — free the slot; leave clients grouped (they idle on a
    silent FIFO) to avoid churn.
- **Reconcile** (idempotent; also fired by control notifications): for each
  active zone, `GetStatus` → find its dongles' clients → `Group.SetClients(group,
  client_ids)` → `Group.SetStream(group, zone's slot stream)`. The target
  `group` is the one `snapserver` currently lists for the zone's first dongle
  client (`snapserver` auto-creates a group per client); `SetClients` then pulls
  the zone's other dongles into it, and `snapserver` deletes the now-empty groups
  they came from.

### 3.5 `Engine` changes (`audio/engine.rs`)

- The shared `snapcast_sink` field is **removed**, replaced by the
  `SnapcastRouter` (constructed I/O-free; `snapserver` still spawns lazily on
  first dongle registration via the router).
- Dongle `Output`s no longer carry a real sink — they're grouped in `snapserver`,
  not decoded into individually. `Output.sink` becomes `Option<Arc<dyn
  AudioSink>>` (or an explicit `kind: Local | Dongle`); only `local` has a sink.
- Zone resolution in `play`:
  - all-local → existing single/FanOut local path (unchanged),
  - any-dongle (enforced all-dongle) → `router.sink_for_zone(...)`,
  - mixed local+dongle → blocked earlier at `set_zone_outputs` (see §4).

---

## 4. Zone-CRUD data model

The per-dongle auto-zones (`id → [id]`) and the synthesized `"default"`
(hub-local) zone **stay**. On top of them the user can define **named
multi-dongle zones**. New engine methods:

- `create_zone(name) -> ZoneId` — generates an id, empty membership. **Duplicate
  names are allowed** (the id is the identifier, not the name).
- `delete_zone(zone)` — stop it, `release_zone`, return its dongles to their own
  auto-zones/groups.
- `rename_zone(zone, name)`.
- `set_zone_outputs(zone, [output_ids])` — the single membership mutator
  (idempotent). Enforces the constraints below.

### Constraints (enforced in `set_zone_outputs`)

- **All-dongle or all-local, never mixed.** A tight-synced zone is dongles-only;
  the hub's local cpal device can't clock-sync with `snapclient`s. Mixed → error.
- The per-dongle auto-zone is an ungrouped dongle's default home.
- **Creating** zones is unbounded; only **playing** zones consume a pool slot, so
  concurrent playback is the thing capped at N.

---

## 5. Wire protocol (iOS ↔ hub)

All new tasks are encrypted + newline-framed exactly like existing tasks, routed
by `commands::dispatch()`, and use the existing `{ status, task, data?, error? }`
response shape.

### New tasks

| task | request `data` | success `data` | errors |
|------|----------------|----------------|--------|
| `create_zone` | `{ "name": "Upstairs" }` | `{ "zone": "<id>" }` | — |
| `delete_zone` | `{ "zone": "<id>" }` | — | `unknown_zone` |
| `rename_zone` | `{ "zone": "<id>", "name": "..." }` | — | `unknown_zone` |
| `set_zone_outputs` | `{ "zone": "<id>", "outputs": ["<dongle-id>", ...] }` | — | `unknown_zone`, `unknown_output`, `mixed_zone_unsupported` |

### Changed/new errors on existing tasks

- `play` gains `no_free_stream` (all N pool slots in use). Existing
  `unknown_zone` / `zone_has_no_outputs` unchanged.

### New live push: `zones`

Additive to the existing flat `outputs` push (which stays for back-compat with
the shipped iOS speaker list). Fired on the same `OUTPUTS_CHANGED` trigger,
carrying full zone membership so a future grouping UI can render it:

```json
{ "status": "ok", "task": "zones",
  "data": { "zones": [
    { "zone": "default", "name": "Hub", "outputs": ["local"], "playing": false },
    { "zone": "<uuid>", "name": "Upstairs", "outputs": ["<d1>","<d2>"], "playing": true } ] } }
```

### iOS

Out of scope for this spec. Every addition above is documented in `CLAUDE.md`'s
protocol section marked **"hub-side shipped, iOS mirror pending (own spec)"** so
the contract is frozen for that follow-up.

---

## 6. Runtime flows

### play → group

1. `play(zone, url)` resolves the zone's **online** outputs.
2. All-local → today's path (lazy `ensure_local`, single/FanOut sink).
3. Any-dongle → `router.sink_for_zone(zone, [dongle ids])`: allocate slot
   (`no_free_stream` if none), record desired state, return `slot.sink`; engine
   spawns the decode thread into it (FIFO write now **blocks** → smooth audio);
   fire a reconcile.
4. **Reconcile** (idempotent; also on every `Client.OnConnect`/`OnDisconnect`):
   per active zone, `GetStatus` → `Group.SetClients` → `Group.SetStream`. A
   late-connecting `snapclient` is pulled onto the right stream automatically.
5. `stop(zone)` shuts the decode pipeline and `router.release_zone(zone)` frees
   the slot; clients stay grouped on the now-idle (silent) FIFO until reassigned.

### resilience

The hub owns intent; the reconciler converges `snapserver` on any trigger. On a
`snapserver` restart the control client reconnects and replays the full desired
state. Audio flows into the FIFO regardless of grouping state, so a transient RPC
failure degrades grouping, not playback.

---

## 7. Error handling

- Pool exhausted → `play` returns `no_free_stream`.
- `snapserver` down / RPC fails → reconcile logs and retries on the next trigger;
  never panics the engine.
- Registered dongle whose `snapclient` hasn't reached `snapserver` yet → not an
  error; the connect notification triggers a later reconcile.
- Mixed-zone attempt → blocked at `set_zone_outputs` time, not play time.

---

## 8. Testing

### Device-free (CI, no `snapserver`/`snapclient`/audio hardware)

- Stream pool: allocate / release / exhaustion.
- Reconciler logic against a **mock `SnapserverControl`**: asserts the correct
  `SetClients`/`SetStream` calls for a given desired state + fake `GetStatus`.
- `SnapserverControl` request/response + notification parsing against a
  **loopback mock snapserver** speaking JSON-RPC lines.
- Engine zone-CRUD + constraint enforcement (mixed-zone rejection, etc.).
- `SnapcastSink` backpressure via a **real temp FIFO + a draining reader thread**
  — proves the `O_NONBLOCK` clear and blocking write with no audio hardware and
  no `snapserver`.

### Demo-gated (`--ignored`, real binaries + hardware)

Extend the existing opt-in path to two streams → two `snapclient`s on two
machines → confirm **independent** audio on each, then group them and confirm
**synchronized** playback. This is the sub-step 3 acceptance demo.

---

## 9. Commit sequencing (each independently shippable)

- **3.1** `SnapcastSink` backpressure fix. Stands alone; improves today's
  single-stream demo immediately.
- **3.2** Multi-stream `SnapserverSupervisor` + stream pool. One zone still works;
  no grouping yet.
- **3.3** `SnapserverControl` JSON-RPC client + notification read (tested against
  the mock snapserver).
- **3.4** `SnapcastRouter` + reconciler; engine routes dongle zones through it.
  Demo: two dongles, independent audio.
- **3.5** Zone-CRUD engine methods + wire tasks + `zones` push. Demo: group two
  dongles → synchronized.

---

## 10. Docs to update as commits land

- `CLAUDE.md` — new tasks/errors, the `zones` push, the desired-state/reconciler
  architecture, dongle `Output.sink` becoming optional, and marking the iOS
  mirror as the next deferred item.
- `docs/multi-room-plan.md` — sub-step 3 marked underway/landed with the same
  detail level as 2.x; retire the manual `Group.SetStream` bring-up workaround
  (now automated by the reconciler).
