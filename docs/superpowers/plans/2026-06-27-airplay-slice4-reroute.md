# AirPlay Slice 4 (Reroute) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the iOS app move a live AirPlay session from one zone to another (`reroute`) while the phone stays connected to the same receiver.

**Architecture:** Add an `Engine::reroute(source, new_zone)` op that composes the existing `detach_driver` / last-wins / lazy-sink machinery: validate the source is active, validate the target zone, **eagerly** resolve the target's sink (so routing errors surface and a failure leaves the source untouched), then swap the source's `dest_zone` and reinstall it as the target zone's driver. The per-chunk PCM pump picks up the new cached sink on its next read — no reader thread restart. `session_ended` resets `dest_zone` to the home zone so reroute is a per-session override. A new `reroute` wire task routes through `commands::dispatch()`.

**Tech Stack:** Rust (workspace binary `audio_share`), `std::sync::Mutex`, `serde_json`. Tests are `cargo test` unit tests in the existing `#[cfg(test)]` modules.

## Global Constraints

- **No device in CI:** unit tests must not open the cpal device or spawn `snapserver`/`shairport-sync`. Anything that resolves a real sink (`zone_sink` success → `ensure_local` or `snapcast.sink_for_zone`) is **demo-gated/manual**, marked `#[ignore]` or left to the end-to-end demo — mirror the existing `#[ignore] engine_plays_default_zone_briefly` test. Device-free tests reach only the validation/error/no-op/revert paths.
- **Lock discipline (copy from `Engine::play`):** snapshot state under a lock, **release the lock before any blocking I/O** (`zone_sink`, which may open cpal or do a snapserver JSON-RPC round-trip, and `detach_driver`, which may join a decode thread), then reacquire to install state. `sources` and `zones` are independent mutexes; never hold `zones` while locking `sources` inside `detach_driver`.
- **Error codes are returned as `String` from engine methods** (e.g. `"unknown_zone"`), exactly like `play`/`set_zone_outputs`; `dispatch()` maps them to `TaskResponse::error(task, code)`.
- **macOS caveat:** the full binary exits reading `/proc/cpuinfo`; run engine unit tests with `cargo test --bin audio_share <name>` (not `--lib`).

---

### Task 1: `Engine::reroute` + revert-to-home (engine-side)

**Files:**
- Modify: `src/audio/engine.rs` — add `reroute` method (near `session_began`/`session_ended`, ~line 555); add one line to `session_ended` (~line 606); add a `#[cfg(test)]` helper `force_dest_zone`; add tests in the `#[cfg(test)] mod tests` block.

**Interfaces:**
- Consumes (already present in `engine.rs`): `self.sources: Mutex<HashMap<ZoneId, SourceState>>` with `SourceState { name, dest_zone, active, routed, sink, .. }`; `self.zones: Mutex<HashMap<ZoneId, ZonePlayback>>` with `ZonePlayback { outputs, current: Option<ZoneDriver>, .. }`; `enum ZoneDriver { Url(Pipeline), Airplay(ZoneId) }`; `fn zone_sink(&self, zone, outputs) -> Result<Arc<dyn AudioSink>, String>`; `fn detach_driver(&self, ZoneDriver)`; `self.snapcast.release_zone(&str)`; `fn notify_sources_changed(&self)`; `fn notify_outputs_changed(&self)`. Test helpers: `add_idle_source(id, name)`, `add_dongle_output(id, name)`, `dongle_offline(id)`, `session_began(id)`, `session_ended(id)`, `zone_has_airplay_driver(zone)`, `list_sources()`.
- Produces: `pub fn reroute(&self, source: &str, new_zone: &str) -> Result<(), String>` — error codes `"unknown_source"`, `"unknown_zone"`, `"zone_has_no_outputs"`, `"mixed_zone_unsupported"`, `"no_free_stream"`. (Consumed by Task 2.)

- [ ] **Step 1: Write the failing tests**

Add these tests inside `mod tests` in `src/audio/engine.rs` (they mirror the existing `session_began_*` tests and need no device):

```rust
#[test]
fn reroute_unknown_source_errors() {
    let engine = Engine::new();
    // No source registered at all.
    assert_eq!(engine.reroute("ghost", "default").unwrap_err(), "unknown_source");
    // A known but inactive source is also "unknown_source" (nothing to move).
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen");
    assert_eq!(engine.reroute("d1", "default").unwrap_err(), "unknown_source");
}

#[test]
fn reroute_unknown_zone_leaves_source_in_place() {
    let engine = Engine::new();
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen");
    engine.session_began("d1"); // active, dest == "d1" (lazy: no sink resolved)

    assert_eq!(engine.reroute("d1", "nope").unwrap_err(), "unknown_zone");

    // Atomic failure: still active, still on its home zone, still driving it.
    let s = &engine.list_sources()[0];
    assert_eq!(s.dest_zone, "d1");
    assert!(s.routed);
    assert!(engine.zone_has_airplay_driver("d1"));
}

#[test]
fn reroute_to_zone_with_no_online_outputs_errors_and_is_atomic() {
    let engine = Engine::new();
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen");
    engine.session_began("d1");

    // d2's auto-zone has only an offline dongle -> zone_has_no_outputs on resolve.
    engine.add_dongle_output("d2", "Bedroom");
    engine.dongle_offline("d2");

    assert_eq!(engine.reroute("d1", "d2").unwrap_err(), "zone_has_no_outputs");

    // Eager-resolution failure left the source untouched on its old zone.
    let s = &engine.list_sources()[0];
    assert_eq!(s.dest_zone, "d1");
    assert!(s.routed);
    assert!(engine.zone_has_airplay_driver("d1"));
    assert!(!engine.zone_has_airplay_driver("d2"));
}

#[test]
fn reroute_to_same_zone_is_noop_ok() {
    let engine = Engine::new();
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen");
    engine.session_began("d1");

    engine.reroute("d1", "d1").expect("same-zone reroute is a no-op Ok");

    let s = &engine.list_sources()[0];
    assert_eq!(s.dest_zone, "d1");
    assert!(s.routed);
}

#[test]
fn session_ended_reverts_rerouted_dest_to_home() {
    let engine = Engine::new();
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen");
    engine.session_began("d1");

    // Simulate a prior reroute having moved the destination off the home zone.
    engine.force_dest_zone("d1", "somewhere-else");
    engine.session_ended("d1");

    // A fresh session reads dest_zone; revert-to-home means it routes home again.
    engine.session_began("d1");
    assert_eq!(engine.list_sources()[0].dest_zone, "d1");
}
```

Also add this test-only helper next to the other `#[cfg(test)]` helpers (`add_idle_source` etc., ~line 762):

```rust
#[cfg(test)]
fn force_dest_zone(&self, source: &str, zone: &str) {
    let mut sources = self.sources.lock().unwrap();
    if let Some(s) = sources.get_mut(source) {
        s.dest_zone = zone.to_string();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin audio_share audio::engine::tests::reroute -- --nocapture` and `cargo test --bin audio_share audio::engine::tests::session_ended_reverts -- --nocapture`
Expected: compile error / FAIL — `no method named reroute` (and `force_dest_zone` resolves once added).

- [ ] **Step 3: Add the revert-to-home line in `session_ended`**

In `Engine::session_ended` (~line 593), inside the block that mutates the source under the `sources` lock (where it already sets `s.active = false; s.routed = false; s.sink = None;` and clears the metadata strings), add the home-zone reset. The source's id **is** its home zone, so:

```rust
s.active = false;
s.routed = false;
s.sink = None;
s.dest_zone = source.to_string(); // revert-to-home: reroute is per-session
// Slice 3: now-playing is session-scoped.
s.title.clear();
s.artist.clear();
s.album.clear();
s.client.clear();
s.art_id.clear();
```

- [ ] **Step 4: Implement `reroute`**

Add this method to `impl Engine` (place it right after `session_began`, before `sink_for_source`):

```rust
/// Reroute a **live** AirPlay `source` to `new_zone`. The phone stays connected
/// to the same receiver; only the internal destination changes. Sink resolution
/// is eager (so routing errors surface here and a failure leaves the source
/// entirely unchanged — atomic), but no reader thread is restarted: the pump
/// picks up the newly cached sink on its next per-chunk `sink_for_source`.
/// Errors: `unknown_source` (no such source or no active session),
/// `unknown_zone`, `zone_has_no_outputs`, `mixed_zone_unsupported`,
/// `no_free_stream`.
pub fn reroute(&self, source: &str, new_zone: &str) -> Result<(), String> {
    // 1. Validate the source is known + active; snapshot its current dest.
    let old_dest = {
        let sources = self.sources.lock().expect("engine sources mutex poisoned");
        let s = sources.get(source).ok_or_else(|| "unknown_source".to_string())?;
        if !s.active {
            return Err("unknown_source".to_string());
        }
        s.dest_zone.clone()
    };

    // 2. Already there: nothing to do (no pushes, no topology churn).
    if new_zone == old_dest {
        return Ok(());
    }

    // 3. Validate the new zone exists; snapshot its outputs off the sources lock.
    let outputs = {
        let zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones
            .get(new_zone)
            .map(|z| z.outputs.clone())
            .ok_or_else(|| "unknown_zone".to_string())?
    };

    // 4. Eager sink resolution off the locks. On failure the source is left
    //    entirely unchanged (still routed to old_dest).
    let sink = self.zone_sink(new_zone, &outputs)?;

    // 5a. Detach this source from its old zone — only if it still drives it
    //     (it may have gone connected-but-unrouted). Free the old Snapcast slot
    //     only when we actually owned that zone.
    let cleared_old = {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        match zones.get_mut(&old_dest) {
            Some(z) if matches!(&z.current, Some(ZoneDriver::Airplay(s)) if s == source) => {
                z.current = None;
                true
            }
            _ => false,
        }
    };
    if cleared_old {
        self.snapcast.release_zone(&old_dest);
    }

    // 5b. Detach whatever currently drives the new zone (last-wins): a URL
    //     pipeline is shut down; another AirPlay source is marked unrouted.
    let new_prev = {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones.get_mut(new_zone).and_then(|z| z.current.take())
    };
    if let Some(prev) = new_prev {
        self.detach_driver(prev);
    }

    // 5c. Point the source at the new zone, cache the resolved sink, install it
    //     as the new zone's driver.
    {
        let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
        if let Some(s) = sources.get_mut(source) {
            s.dest_zone = new_zone.to_string();
            s.routed = true;
            s.sink = Some(Arc::clone(&sink));
        }
    }
    {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        if let Some(z) = zones.get_mut(new_zone) {
            z.current = Some(ZoneDriver::Airplay(source.to_string()));
        }
    }

    self.notify_sources_changed();
    self.notify_outputs_changed();
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --bin audio_share audio::engine::tests::reroute && cargo test --bin audio_share audio::engine::tests::session_ended_reverts`
Expected: PASS (5 tests). Also run the whole module to catch regressions: `cargo test --bin audio_share audio::engine::tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/audio/engine.rs
git commit -m "AirPlay slice 4: Engine::reroute + revert-to-home on session end

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Wire the `reroute` task through `dispatch`

**Files:**
- Modify: `src/server/commands.rs` — add `Task::Reroute` to the enum, `parse`, and `name`; add a `Task::Reroute` arm in `dispatch`; add dispatch tests.

**Interfaces:**
- Consumes: `ENGINE.reroute(source, zone) -> Result<(), String>` (Task 1); `TaskResponse::{accepted, error}`; `Value` payload indexing as used by the `play`/`delete_zone` arms.
- Produces: recognized wire task `"reroute"`. Request `data: { "source": "<id>", "zone": "<id>" }`; success `{ "status": "ok", "task": "reroute" }`. New error codes `missing_source`, `missing_zone`, `unknown_source` (others reused from the engine string).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/server/commands.rs` (these are device-free — they only reach the missing-field guards, which return before touching the engine):

```rust
#[test]
fn parses_reroute_task() {
    assert_eq!(Task::parse("reroute"), Task::Reroute);
}

#[test]
fn reroute_without_source_errors_missing_source() {
    let json = dispatch(Task::Reroute, &json!({ "zone": "office" })).to_json();
    assert!(json.contains("\"status\":\"error\""));
    assert!(json.contains("\"error\":\"missing_source\""));
    assert!(json.contains("\"task\":\"reroute\""));
}

#[test]
fn reroute_without_zone_errors_missing_zone() {
    let json = dispatch(Task::Reroute, &json!({ "source": "kitchen" })).to_json();
    assert!(json.contains("\"status\":\"error\""));
    assert!(json.contains("\"error\":\"missing_zone\""));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin audio_share server::commands::tests::reroute -- --nocapture` and `cargo test --bin audio_share server::commands::tests::parses_reroute -- --nocapture`
Expected: compile error — `no variant named Reroute`.

- [ ] **Step 3: Add the `Reroute` variant**

In `enum Task` (~line 9) add `Reroute,` (place it after `GetArt`, grouping it with the other engine tasks). In `Task::parse` (~line 33) add:

```rust
"reroute" => Task::Reroute,
```

In `Task::name` (~line 52) add:

```rust
Task::Reroute => "reroute",
```

- [ ] **Step 4: Add the `dispatch` arm**

In `dispatch` (`src/server/commands.rs`), add this arm alongside the other engine tasks (e.g. right after the `Task::SetZoneOutputs` arm). It reads both required fields (treating empty string as absent, like `play`'s url check), then maps the engine's `String` error to the wire code. `unknown_zone`/`zone_has_no_outputs`/`no_free_stream`/`mixed_zone_unsupported`/`unknown_source` already match the engine's strings, so they pass through:

```rust
Task::Reroute => {
    let source = data["source"].as_str().filter(|s| !s.is_empty());
    let zone = data["zone"].as_str().filter(|z| !z.is_empty());
    match (source, zone) {
        (None, _) => TaskResponse::error("reroute", "missing_source"),
        (Some(_), None) => TaskResponse::error("reroute", "missing_zone"),
        (Some(source), Some(zone)) => match ENGINE.reroute(source, zone) {
            Ok(()) => {
                println!("Rerouted source {} -> zone {}", source, zone);
                TaskResponse::accepted("reroute", None)
            }
            Err(code) => {
                println!("Reroute {} -> {} failed: {}", source, zone, code);
                TaskResponse::error("reroute", code)
            }
        },
    }
}
```

> Note: the top of `dispatch` computes a default `zone` from `data["zone"]`, but this arm reads `data["zone"]` directly (a reroute with no zone is an error, not a default-to-"default"). That outer `zone` binding is unused by this arm — fine, it's used by `play`/`stop`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --bin audio_share server::commands::tests`
Expected: PASS (including the three new tests). Then a full build to confirm the `Err(code)` lifetime (the engine returns an owned `String`, which `TaskResponse::error` accepts — `delete_zone`/`set_zone_outputs` do the same): `cargo build --bin audio_share`
Expected: builds clean.

- [ ] **Step 6: Commit**

```bash
git add src/server/commands.rs
git commit -m "AirPlay slice 4: reroute task routed through dispatch

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Freeze the wire contract + mark Slice 4 done in docs

**Files:**
- Modify: `CLAUDE.md` — recognized-tasks list, error taxonomy, the AirPlay status paragraph; add the `reroute` request/response shape.
- Modify: `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md` — mark Slice 4 `[DONE]` with an as-built note.

No code/tests in this task — it's the documentation deliverable that closes the slice. Do it as one commit.

- [ ] **Step 1: Update the recognized-tasks list in `CLAUDE.md`**

In the "Cross-project wire protocol" section, the sentence listing recognized tasks currently ends `… list_sources, get_art`. Add `reroute`:

> Recognized tasks: `play`, `pause`, `stop`, `seek`, `volume`, `list_outputs`, `list_zones`, `create_zone`, `delete_zone`, `rename_zone`, `set_zone_outputs`, `list_sources`, `get_art`, `reroute`.

- [ ] **Step 2: Add the `reroute` task shape to `CLAUDE.md`**

After the "Album art fetch (`get_art`)" subsection, add:

```markdown
**Reroute a live AirPlay session (`reroute`) — iOS → server.** Moves an active
source's audio to a different zone while the phone stays connected to the same
receiver (the receiver's AirPlay/Bonjour name does **not** change — the app's
`dest_zone` is the source of truth for where it's playing):
`{ "task": "reroute", "data": { "source": "<home-zone-id>", "zone": "<dest-zone-id>" }, "session_token": "<UUID>" }`
→ `{ "status": "ok", "task": "reroute" }`. Routed through `commands::dispatch()`.
Reroute is a per-session override: `dest_zone` reverts to the source's home zone
when the session ends. Errors: `missing_source`, `missing_zone`, `unknown_source`
(no such source or no active session), and the `play`-style routing codes
`unknown_zone` / `zone_has_no_outputs` / `mixed_zone_unsupported` / `no_free_stream`
(reroute resolves the target sink eagerly through the same `zone_sink()` path, so a
failure leaves the source unchanged).
```

- [ ] **Step 3: Extend the error taxonomy line in `CLAUDE.md`**

In the "Error codes so far" paragraph, add `unknown_source` (and the two missing-field codes) next to `unknown_art`:

> … `unknown_art` (`get_art` with an `art_id` not in the cache …), `unknown_source` (`reroute` referenced a source with no active session, or an unknown receiver), `missing_source` / `missing_zone` (`reroute` with no `data.source` / `data.zone`).

- [ ] **Step 4: Update the AirPlay status paragraph in `CLAUDE.md`**

Find the sentence ending the AirPlay Slice 3 description: `Reroute (Slice 4) is not yet built.` (it appears in the Slice 2 and Slice 3 summaries). Replace each occurrence with:

> **AirPlay Slice 4 is in:** the `reroute` task + `Engine::reroute` move a live source to another zone (eager `zone_sink` resolution → atomic on failure; the per-chunk pump picks up the new sink with no reader restart), and `session_ended` reverts `dest_zone` to the home zone so reroute is per-session. The receiver's Bonjour name is unchanged by a reroute.

- [ ] **Step 5: Mark Slice 4 done in the AirPlay design spec**

In `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md`, change the `- **Slice 4 — Reroute.**` bullet (~line 319) to `- **Slice 4 — Reroute. [DONE]**` and append an as-built note matching the Slice 2/3 note style:

```markdown
  > **As built — Slice 4:** reroute is **cache invalidation on the pull-driven
  > pump, not a reader restart** — the pump's per-chunk `sink_for_source` picks up
  > the rerouted zone's sink (rebuilding the resampler if the format differs).
  > Sink resolution is **eager** (errors surface in the `reroute` response; a
  > failure leaves the source on its old zone), and `session_ended` **reverts
  > `dest_zone` to the home zone** so a reroute does not persist across sessions.
```

- [ ] **Step 6: Verify the build and committed-doc consistency, then commit**

Run: `cargo test --bin audio_share audio::engine::tests server::commands::tests`
Expected: PASS (no code changed, but confirms the tree still builds/tests green before the doc commit).

```bash
git add CLAUDE.md docs/superpowers/specs/2026-06-23-airplay-receiver-design.md
git commit -m "docs: freeze reroute wire contract, mark AirPlay Slice 4 done

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Notes on testing scope (read before Task 1)

The **happy path** — a reroute that resolves a real sink and you hear audio move from Kitchen to Living Room while the phone stays connected — needs `shairport-sync`, a real AirPlay sender, and audio hardware, so it is **demo-gated** (manual), exactly like the existing `#[ignore] engine_plays_default_zone_briefly` smoke test. Do **not** write a unit test that calls `reroute` into a path where `zone_sink` succeeds — that opens cpal or spawns `snapserver` and will hang/fail in CI. The device-free tests in Task 1 deliberately stop at validation/eager-resolution-failure/no-op/revert, which together cover every branch reachable without hardware (the last-wins install + old-slot release on the success path reuse `detach_driver` / `release_zone`, already covered by the existing `url_play_detaches_an_airplay_source_last_wins` and Snapcast tests).

**Manual demo checklist (post-merge, on the Pi):**
1. `play`/AirPlay to the Kitchen zone; confirm audio.
2. Send `reroute { source: "<kitchen-zone-id>", zone: "<living-room-zone-id>" }`.
3. Confirm audio moves to Living Room and the iPhone's AirPlay menu still shows "Kitchen".
4. Disconnect; reconnect to Kitchen; confirm it plays in Kitchen again (revert-to-home).
