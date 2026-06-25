# AirPlay Receiver — Slice 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn every Audio Share zone into its own classic-AirPlay target; when a phone AirPlays to a zone, route that audio to the zone's outputs (the hub's local speaker *or* a dongle group via Snapcast), and surface "this zone is receiving" to the app.

**Architecture:** The engine gains a logical **source registry** alongside zones (one source per zone, id == home-zone id) and a `ShairportManager` that **reconciles a supervised `shairport-sync` process per zone** against the live zone set (spawn on `create_zone`/dongle-register, kill on `delete_zone`, restart-renamed on `rename_zone`). Each receiver runs a continuous **pump thread** that uses the **audio FIFO itself as the session bracket** — a blocking FIFO open means a session started, EOF means it ended (reusing Slice 1's `pump_one_session`). Per audio chunk the pump asks the engine *"where does this source write right now?"* via a `SessionSink` seam, so routing (and, later, reroute) is resolved live through the existing `zone_sink()` — a dongle zone "just works" because its sink is a snapserver FIFO. A `ZoneDriver` enum generalizes a zone's `current` playback to **URL-or-AirPlay, one driver per zone, last-wins**.

**Tech Stack:** Rust (edition 2021), `libc` (FIFO `mkfifo`), `rubato` (resampling via `decode.rs`), `shairport-sync` (external, **classic** build), `tokio::sync::broadcast` (push eventing), std `process`/`thread`/`fs`/`sync`.

## Global Constraints

- Rust edition 2021; recent stable toolchain (≥1.85; 1.96 in use) — copied from CLAUDE.md.
- AirPlay PCM is fixed **44100 Hz, 16-bit, 2 channels** (`s16le` interleaved) — reuse `airplay::AIRPLAY_SAMPLE_RATE` / `AIRPLAY_CHANNELS`.
- **Classic (non-AirPlay-2) `shairport-sync` only.** AirPlay 2 cannot run multiple instances per host; classic can, given each instance a **unique RTSP port** and a **unique `airplay_device_id`**. (AirPlay 2 / NQPTP is endgame "3a", not this slice.)
- **Session detection is the audio FIFO** (blocking open = `pbeg`, EOF = `pend`). This is a **deliberate deviation** from the design doc's "metadata pipe" decision: it reuses Slice 1's `pump_one_session`, avoids interrupting a blocked PCM reader, and makes routing per-chunk (reroute-ready). The metadata pipe returns in **Slice 3** for track title/artist/album + art. Record this deviation in the spec (Task 8).
- **One driver per zone, last-wins.** Starting a URL on a zone an AirPlay source feeds detaches that source (it goes *connected-but-unrouted*, discarding PCM until its session ends); a new AirPlay session on a zone playing a URL shuts the URL pipeline. Mirrors `play`/`stop`'s existing per-zone shutdown.
- **Lock discipline (mirror the existing engine):** snapshot under the `zones`/`sources` lock, **release before any blocking I/O** (spawning a process, `zone_sink()`'s snapserver JSON-RPC / cpal open, joining a thread), then reacquire to install state. `sources` is an independent mutex from `zones`.
- New code adds **device-free unit tests** (CI-safe: no audio hardware, no `shairport-sync`, no snapserver). Anything that opens a real device / spawns the binary is **demo-gated** with `#[ignore]`, mirroring `audio::snapcast::tests` and `audio::airplay::tests::receives_airplay_briefly`.
- Mirror module style: module-level `//!` doc, `const` for fixed values, `Drop`-based process cleanup, pure helpers split out for testing, trait seams for device-free tests (as `SnapcastControl` / `DongleRegistrar` already do).

---

### Task 1: Per-instance receiver identity (unique port + device id)

Classic `shairport-sync` instances collide unless each has a distinct RTSP port and AirPlay device id. Extend Slice 1's supervisor/config to derive both from a stable **slot** index, so the manager (Task 4) can assign one slot per zone.

**Files:**
- Modify: `src/audio/airplay.rs` (extend `shairport_config`, add `RTSP_PORT_BASE`, add `ShairportSupervisor::spawn_for_slot`)
- Test: inline `#[cfg(test)]` in `src/audio/airplay.rs`

**Interfaces:**
- Consumes (existing, Slice 1): `fn shairport_config(name: &str, port: u16, fifo_path: &Path) -> String`, `ShairportSupervisor::spawn_with(binary, name, port, fifo) -> Result<Self, String>`, `fn fifo_path(index: usize) -> PathBuf`.
- Produces: `pub const RTSP_PORT_BASE: u16 = 5000;`
- Produces: `pub fn slot_port(slot: usize) -> u16` — `RTSP_PORT_BASE + slot`.
- Produces: `pub fn slot_device_id(slot: usize) -> String` — a stable, unique-per-slot 12-hex-char id, e.g. `format!("AA5500{:06X}", slot)`.
- Produces: `ShairportSupervisor::spawn_for_slot(name: &str, slot: usize) -> Result<Self, String>` — production entry point; resolves port/device-id/fifo from `slot`, spawns the real `shairport-sync` binary.
- Changed signature: `fn shairport_config(name: &str, port: u16, device_id: &str, fifo_path: &Path) -> String` (adds `device_id`).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/audio/airplay.rs`:

```rust
#[test]
fn slot_maps_to_unique_port_and_device_id() {
    assert_eq!(slot_port(0), 5000);
    assert_eq!(slot_port(3), 5003);
    // Device ids are stable and distinct per slot.
    assert_ne!(slot_device_id(0), slot_device_id(1));
    assert_eq!(slot_device_id(0), slot_device_id(0));
    assert_eq!(slot_device_id(0).len(), 12);
}

#[test]
fn config_includes_device_id() {
    let cfg = shairport_config("Kitchen", 5002, "AA5500000002", Path::new("/tmp/x.pcm"));
    assert!(cfg.contains("name = \"Kitchen\""), "{cfg}");
    assert!(cfg.contains("port = 5002"), "{cfg}");
    assert!(cfg.contains("airplay_device_id = \"AA5500000002\""), "{cfg}");
    assert!(cfg.contains("name = \"/tmp/x.pcm\""), "{cfg}"); // pipe.name
}
```

- [ ] **Step 2: Update the existing config test for the new signature**

The Slice 1 test `config_sets_name_port_and_pipe` calls `shairport_config` with the old 3-arg signature. Update its call:

```rust
let cfg = shairport_config("Audio Share (Hub)", 5000, "AA5500000000", Path::new("/tmp/x.pcm"));
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p audio_share --bin audioshare_device audio::airplay -- --nocapture`
Expected: FAIL — `slot_port`/`slot_device_id` not found; `shairport_config` arity mismatch.

> Note: the device server binary's package is `audio_share`; if `-p audio_share` errors, use `cargo test --bin audioshare_device audio::airplay`. Confirm the exact bin name with `cargo metadata --no-deps --format-version 1 | tr ',' '\n' | grep -i name`.

- [ ] **Step 4: Implement**

Add the constants/helpers near the top of `src/audio/airplay.rs` (after the existing `const` block):

```rust
/// Base RTSP port for classic shairport-sync instances; instance `slot` uses
/// `RTSP_PORT_BASE + slot`. Classic AirPlay needs a distinct port per instance.
pub const RTSP_PORT_BASE: u16 = 5000;

/// RTSP port for receiver `slot`.
pub fn slot_port(slot: usize) -> u16 {
    RTSP_PORT_BASE + slot as u16
}

/// A stable, unique-per-slot AirPlay device id (12 hex chars, the classic
/// `airplay_device_id` format). Distinct ids stop AirPlay 1 clients from
/// conflating instances at the same IP.
pub fn slot_device_id(slot: usize) -> String {
    format!("AA5500{:06X}", slot)
}
```

Change `shairport_config` to take and emit the device id:

```rust
fn shairport_config(name: &str, port: u16, device_id: &str, fifo_path: &Path) -> String {
    format!(
        "general =\n{{\n  name = \"{name}\";\n  port = {port};\n  airplay_device_id = \"{device_id}\";\n}};\n\n\
         pipe =\n{{\n  name = \"{}\";\n}};\n",
        fifo_path.display()
    )
}
```

Update `spawn_with` to pass a device id through. Give it a `device_id` parameter and thread it into the config write:

```rust
pub fn spawn_with(
    binary: impl Into<String>,
    name: &str,
    port: u16,
    device_id: &str,
    fifo: &Path,
) -> Result<Self, String> {
    let binary = binary.into();
    ensure_fifo(fifo)?;

    let config_path = PathBuf::from(format!("{CONFIG_PATH_BASE}-{port}.conf"));
    std::fs::write(&config_path, shairport_config(name, port, device_id, fifo))
        .map_err(|e| format!("failed to write shairport config {}: {e}", config_path.display()))?;

    let first = spawn_shairport(&binary, &config_path)?;

    let stop = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(Some(first)));

    let monitor = {
        let stop = Arc::clone(&stop);
        let child = Arc::clone(&child);
        let binary = binary.clone();
        let config_path = config_path.clone();
        thread::Builder::new()
            .name("shairport-supervisor".to_string())
            .spawn(move || monitor_loop(&binary, &config_path, &stop, &child))
            .map_err(|e| format!("failed to spawn shairport supervisor thread: {e}"))?
    };

    Ok(Self { stop, child, monitor: Some(monitor) })
}
```

Update the existing `spawn` to pass a default device id, and add the slot-based entry point:

```rust
/// Spawn `shairport-sync` (resolved from `PATH`) as a receiver named `name`
/// on `port`, writing PCM to `fifo`, with device id `device_id`.
pub fn spawn(name: &str, port: u16, device_id: &str, fifo: &Path) -> Result<Self, String> {
    Self::spawn_with("shairport-sync", name, port, device_id, fifo)
}

/// Production entry: spawn the receiver for `slot`, deriving port, device id,
/// and audio FIFO from the slot index.
pub fn spawn_for_slot(name: &str, slot: usize) -> Result<Self, String> {
    Self::spawn(name, slot_port(slot), &slot_device_id(slot), &fifo_path(slot))
}
```

Update the Slice 1 `#[ignore]` test `receives_airplay_briefly` to call the new API (it currently calls `ShairportSupervisor::spawn("Audio Share (Hub)", 5000, &fifo)`):

```rust
let _server = ShairportSupervisor::spawn_for_slot("Audio Share (Hub)", 0)
    .expect("shairport-sync should spawn (is the classic build installed?)");
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --bin audioshare_device audio::airplay`
Expected: PASS (all airplay tests, including the updated config test).

- [ ] **Step 6: Commit**

```bash
git add src/audio/airplay.rs
git commit -m "AirPlay slice 2: per-instance receiver identity (slot -> port + device id)"
```

---

### Task 2: Continuous receiver loop with a live sink resolver

Slice 1's `pump_one_session` resolves a *single* sink up front. Slice 2 needs the pump to (a) loop across sessions, (b) signal session begin/end, and (c) ask **per chunk** where to write — so a detached/rerouted source discards or follows its destination. Add `run_receiver` and refactor `pump_one_session` to accept an already-open file plus a per-chunk resolver.

**Files:**
- Modify: `src/audio/airplay.rs`
- Test: inline `#[cfg(test)]` in `src/audio/airplay.rs`

**Interfaces:**
- Consumes (existing): `i16le_to_planar_f32`, `ResamplePipeline`, `mix_planar`, `AIRPLAY_SAMPLE_RATE`, `AIRPLAY_CHANNELS`.
- Produces:
  ```rust
  pub fn run_receiver(
      fifo: &Path,
      stop: &Arc<AtomicBool>,
      mut began: impl FnMut(),
      mut resolve: impl FnMut() -> Option<Arc<dyn AudioSink>>,
      mut ended: impl FnMut(),
  ) -> Result<(), String>
  ```
  Loops until `stop`: blocking-open `fifo` (returns when a sender connects → session start), call `began()`, pump the open FIFO to EOF — per chunk calling `resolve()`; `Some(sink)` → convert+resample+write into it, `None` → discard the chunk — then call `ended()`. Resampling is rebuilt whenever the resolved sink's format differs from the pipeline's current target.
- Produces (used by tests): `pump_open(file, stop, resolve) -> Result<(), String>` — pumps one already-open FIFO `File` to EOF using the per-chunk resolver. `run_receiver` opens the FIFO and delegates here.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module. It exercises a real FIFO (CI-safe — no shairport, no audio device): a writer feeds known `s16le` bytes, the resolver returns a `Capture` sink at AirPlay's native rate (passthrough), and we assert begin/end fired once and samples arrived.

```rust
#[test]
fn run_receiver_brackets_session_and_routes_chunks() {
    use std::sync::atomic::{AtomicUsize, Ordering as O};

    struct Capture(std::sync::Mutex<Vec<f32>>);
    impl AudioSink for Capture {
        fn sample_rate(&self) -> u32 { AIRPLAY_SAMPLE_RATE }
        fn channels(&self) -> u16 { AIRPLAY_CHANNELS as u16 }
        fn write(&self, samples: &[f32]) { self.0.lock().unwrap().extend_from_slice(samples); }
    }

    let path = std::env::temp_dir().join(format!("as-airplay-run-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo failed");

    let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 0.25];
    let mut bytes = Vec::new();
    for &s in &samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }

    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&writer_path).unwrap();
        f.write_all(&bytes).unwrap();
        // drop -> EOF -> session end
    });

    let sink = Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
    let stop = Arc::new(AtomicBool::new(false));
    let begins = Arc::new(AtomicUsize::new(0));
    let ends = Arc::new(AtomicUsize::new(0));

    let reader_path = path.clone();
    let reader_stop = Arc::clone(&stop);
    let reader_sink = Arc::clone(&sink);
    let reader_begins = Arc::clone(&begins);
    let reader_ends = Arc::clone(&ends);
    let reader = std::thread::spawn(move || {
        // Stop after the first session so the loop terminates.
        let one_shot = Arc::clone(&reader_stop);
        let _ = run_receiver(
            &reader_path,
            &reader_stop,
            || { reader_begins.fetch_add(1, O::Relaxed); },
            || Some(reader_sink.clone() as Arc<dyn AudioSink>),
            || { reader_ends.fetch_add(1, O::Relaxed); one_shot.store(true, O::Relaxed); },
        );
    });

    writer.join().unwrap();
    reader.join().unwrap();

    assert_eq!(begins.load(O::Relaxed), 1, "began fired once");
    assert_eq!(ends.load(O::Relaxed), 1, "ended fired once");
    let got = sink.0.lock().unwrap().clone();
    assert_eq!(got.len(), samples.len(), "all frames delivered");
    for (g, s) in got.iter().zip(samples.iter()) {
        assert!((g - s).abs() < 1e-3, "passthrough mismatch: {g} vs {s}");
    }
}

#[test]
fn run_receiver_discards_when_unrouted() {
    let path = std::env::temp_dir().join(format!("as-airplay-discard-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo failed");

    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&writer_path).unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    });

    let stop = Arc::new(AtomicBool::new(false));
    let reader_path = path.clone();
    let reader_stop = Arc::clone(&stop);
    let reader = std::thread::spawn(move || {
        let one_shot = Arc::clone(&reader_stop);
        // resolve always None: every chunk is discarded, must not panic, must EOF cleanly.
        let _ = run_receiver(
            &reader_path,
            &reader_stop,
            || {},
            || None,
            || { one_shot.store(true, std::sync::atomic::Ordering::Relaxed); },
        );
    });

    writer.join().unwrap();
    reader.join().unwrap();
    let _ = std::fs::remove_file(&path);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin audioshare_device audio::airplay::tests::run_receiver`
Expected: FAIL — `run_receiver` not found.

- [ ] **Step 3: Implement `pump_open` and `run_receiver`; retire `pump_one_session`**

Replace `pump_one_session` (it resolved a single fixed sink) with `pump_open` (already-open file + per-chunk resolver) and add `run_receiver`. Keep `pump_fifo_to_sink` working by reimplementing it on top of the new core (so the Slice 1 demo test still compiles).

```rust
/// Pump one already-open AirPlay FIFO `file` to EOF. Per chunk, `resolve()`
/// returns the sink to write into right now (or `None` to discard — the source
/// is connected-but-unrouted). The resample pipeline is (re)built whenever the
/// resolved sink's format changes. Returns `Ok(())` on a clean EOF (session end).
fn pump_open(
    mut file: File,
    stop: &Arc<AtomicBool>,
    resolve: &mut impl FnMut() -> Option<Arc<dyn AudioSink>>,
) -> Result<(), String> {
    let frame_bytes = AIRPLAY_CHANNELS * 2;
    let mut remainder: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    // Lazily built per (sample_rate, channels) of the currently resolved sink.
    let mut pipeline: Option<(u32, u16, ResamplePipeline)> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let n = match file.read(&mut buf) {
            Ok(0) => return Ok(()), // writer closed: session ended
            Ok(n) => n,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("airplay fifo read error: {e}")),
        };

        remainder.extend_from_slice(&buf[..n]);
        let whole = (remainder.len() / frame_bytes) * frame_bytes;
        if whole == 0 {
            continue;
        }

        // Where do we write this chunk right now?
        let Some(sink) = resolve() else {
            remainder.drain(..whole); // unrouted: discard, stay session-active
            continue;
        };

        // (Re)build the pipeline if the sink's format changed.
        let need_rebuild = !matches!(
            &pipeline,
            Some((sr, ch, _)) if *sr == sink.sample_rate() && *ch == sink.channels()
        );
        if need_rebuild {
            let p = ResamplePipeline::new(
                AIRPLAY_SAMPLE_RATE,
                AIRPLAY_CHANNELS,
                sink.sample_rate(),
                sink.channels() as usize,
            )?;
            pipeline = Some((sink.sample_rate(), sink.channels(), p));
        }

        let planar = i16le_to_planar_f32(&remainder[..whole], AIRPLAY_CHANNELS);
        remainder.drain(..whole);
        let mixed = mix_planar(&planar, sink.channels() as usize);
        if let Some((_, _, p)) = pipeline.as_mut() {
            p.push_and_drain(mixed, sink.as_ref());
        }
    }
}

/// Continuously receive AirPlay sessions from `fifo` until `stop`. Each blocking
/// FIFO open is a session start (`began`); EOF is its end (`ended`). While a
/// session runs, `resolve` decides per chunk where the audio goes (live routing,
/// including unrouted = discard). Mirrors the URL decode loop but pull-driven by
/// the sender.
pub fn run_receiver(
    fifo: &Path,
    stop: &Arc<AtomicBool>,
    mut began: impl FnMut(),
    mut resolve: impl FnMut() -> Option<Arc<dyn AudioSink>>,
    mut ended: impl FnMut(),
) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        // Blocking open: returns once a sender (shairport) opens the write end.
        let file = match File::open(fifo) {
            Ok(f) => f,
            Err(e) => return Err(format!("open airplay fifo {} failed: {e}", fifo.display())),
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        began();
        let result = pump_open(file, stop, &mut resolve);
        ended();
        result?;
    }
    Ok(())
}
```

Reimplement `pump_fifo_to_sink` on top of the new core so the Slice 1 `#[ignore]` test keeps working:

```rust
/// Convenience wrapper (used by the Slice 1 demo test): pump `fifo` into a single
/// fixed `sink` across sessions until `stop`.
pub fn pump_fifo_to_sink(
    fifo: &Path,
    sink: &Arc<dyn AudioSink>,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    let sink = Arc::clone(sink);
    run_receiver(fifo, stop, || {}, move || Some(Arc::clone(&sink)), || {})
}
```

Update the Slice 1 `receives_airplay_briefly` test's `pump_fifo_to_sink` call to pass an `Arc<dyn AudioSink>` (it currently passes `pump_output.as_ref()`):

```rust
let sink: Arc<dyn AudioSink> = output.clone();
let pump_stop = Arc::clone(&stop);
let pump_fifo = fifo.clone();
let worker = thread::spawn(move || {
    pump_fifo_to_sink(&pump_fifo, &sink, &pump_stop)
});
```

Remove the now-unused `pump_passes_through_at_native_rate` test's reliance on `pump_one_session`: either delete that test (its coverage is subsumed by `run_receiver_brackets_session_and_routes_chunks`) or rewrite its single call to use `run_receiver`. Delete it to stay DRY.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin audioshare_device audio::airplay`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay.rs
git commit -m "AirPlay slice 2: continuous receiver loop with per-chunk sink resolver"
```

---

### Task 3: Engine source model — `ZoneDriver`, source registry, session hooks

Generalize a zone's `current` to a `ZoneDriver` (URL *or* AirPlay) and add a logical **source registry**. Add the `SessionSink` methods the pump thread calls. Keep blocking I/O (`zone_sink`, pipeline shutdown) off the locks.

**Files:**
- Modify: `src/audio/engine.rs`
- Test: inline `#[cfg(test)]` in `src/audio/engine.rs`

**Interfaces:**
- Consumes (existing): `zone_sink`, `Pipeline`, `ZonePlayback`, `OUTPUTS_CHANGED`, `SnapcastRouter::release_zone`.
- Produces:
  ```rust
  enum ZoneDriver { Url(Pipeline), Airplay(ZoneId) } // ZonePlayback.current: Option<ZoneDriver>
  struct SourceState { name: String, dest_zone: ZoneId, active: bool, routed: bool, sink: Option<Arc<dyn AudioSink>> }
  // Engine fields: sources: Mutex<HashMap<ZoneId, SourceState>>
  ```
- Produces (the `SessionSink` surface, called by the pump thread):
  ```rust
  pub fn session_began(&self, source: &str);
  pub fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>>;
  pub fn session_ended(&self, source: &str);
  pub fn list_sources(&self) -> Vec<SourceView>; // active sources only
  ```
- Produces: `pub struct SourceView { pub source: ZoneId, pub name: String, pub dest_zone: ZoneId, pub routed: bool }`.

- [ ] **Step 1: Write the failing tests (device-free)**

The success path of `sink_for_source` resolves a real sink (`zone_sink` → cpal/snapserver) and is demo-gated; these unit tests cover only the device-free bookkeeping. They use a helper that injects an idle source and a dummy URL pipeline (a trivial thread that just watches the stop flag — no audio device).

```rust
// A stand-in decode pipeline: a thread that idles until its stop flag is set,
// so we can test driver conflict/shutdown without opening an audio device.
fn dummy_pipeline() -> Pipeline {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::Builder::new()
        .name("dummy-pipeline".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(1));
            }
        })
        .unwrap();
    Pipeline { stop, handle }
}

#[test]
fn session_began_then_ended_tracks_active_and_clears_driver() {
    let engine = Engine::new();
    // Register an idle source for a dongle zone (no device needed to begin).
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen"); // test helper, see Step 3

    engine.session_began("d1");
    let active = engine.list_sources();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].source, "d1");
    assert_eq!(active[0].dest_zone, "d1");
    assert!(active[0].routed);
    // The zone now has an Airplay driver.
    assert!(engine.zone_has_airplay_driver("d1")); // test helper

    engine.session_ended("d1");
    assert!(engine.list_sources().is_empty(), "ended -> not active");
    assert!(!engine.zone_has_airplay_driver("d1"), "driver cleared");
}

#[test]
fn url_play_detaches_an_airplay_source_last_wins() {
    let engine = Engine::new();
    engine.add_dongle_output("d1", "Kitchen");
    engine.add_idle_source("d1", "Kitchen");
    engine.session_began("d1");
    assert!(engine.list_sources()[0].routed);

    // Simulate a URL taking over the same zone (device-free: inject a dummy
    // pipeline as the new driver via the same detach path play() uses).
    engine.install_url_driver_for_test("d1", dummy_pipeline());

    // The source is still session-active but now unrouted (discarding).
    let s = &engine.list_sources()[0];
    assert!(s.active_but_unrouted());
    // Cleanup: stop the zone (shuts the dummy pipeline).
    engine.stop("d1");
}

#[test]
fn session_began_on_unknown_source_is_noop() {
    let engine = Engine::new();
    engine.session_began("ghost");
    assert!(engine.list_sources().is_empty());
}
```

For `SourceView`, add a tiny convenience used above:

```rust
impl SourceView {
    fn active_but_unrouted(&self) -> bool { !self.routed }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin audioshare_device audio::engine::tests::session`
Expected: FAIL — `ZoneDriver`, `sources`, `session_began`, helpers not found.

- [ ] **Step 3: Implement**

Add the types and field. Change `ZonePlayback.current` from `Option<Pipeline>` to `Option<ZoneDriver>`:

```rust
/// What currently drives a zone's audio. One driver per zone, last-wins.
enum ZoneDriver {
    /// A URL decode pipeline (internet radio etc.).
    Url(Pipeline),
    /// An AirPlay source (by its id == home-zone id) is feeding this zone.
    Airplay(ZoneId),
}

/// Logical state of one AirPlay receiver/source. The OS process + pump thread
/// live in the ShairportManager; this is the routing/session view the engine and
/// the `sources` push need.
struct SourceState {
    name: String,
    dest_zone: ZoneId, // Slice 2: always == the source id (reroute is Slice 4)
    active: bool,      // a session is in progress (FIFO open)
    routed: bool,      // currently dest_zone's driver (false = connected-but-unrouted)
    sink: Option<Arc<dyn AudioSink>>, // cached resolved sink while active+routed
}

/// A source as reported to clients (active sessions only).
pub struct SourceView {
    pub source: ZoneId,
    pub name: String,
    pub dest_zone: ZoneId,
    pub routed: bool,
}
```

Add the `sources` field to `Engine` and initialize it in `new()`:

```rust
// in struct Engine:
sources: Mutex<HashMap<ZoneId, SourceState>>,

// in Engine::new(), in the struct literal:
sources: Mutex::new(HashMap::new()),
```

Update `ZonePlayback` construction sites: it currently sets `current: None` — no change to those. Update `play`, `stop`, `delete_zone`, and `list_zones` for the new `current` type.

In `play`, the existing `if let Some(pipeline) = zone_state.current.take() { pipeline.shutdown(); }` becomes a detach that also handles an Airplay driver:

```rust
// Replace the take/shutdown block in play() with:
if let Some(prev) = zone_state.current.take() {
    self.detach_driver(prev);
}
// ... after building the pipeline:
zone_state.current = Some(ZoneDriver::Url(Pipeline { stop, handle }));
```

`stop` similarly:

```rust
pub fn stop(&self, zone: &str) {
    let prev = {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones.get_mut(zone).and_then(|z| z.current.take())
    };
    if let Some(prev) = prev {
        self.detach_driver(prev);
    }
    self.snapcast.release_zone(zone);
}
```

`delete_zone`'s `if let Some(pipeline) = removed.current { pipeline.shutdown(); }` becomes:

```rust
if let Some(driver) = removed.current {
    self.detach_driver(driver);
}
```

`list_zones`'s `playing: z.current.is_some()` is unchanged (it's still `Option<_>`).

Add the detach helper and the session hooks:

```rust
/// Tear down a zone's previous driver. A URL pipeline is shut down; an AirPlay
/// source is marked unrouted (its pump keeps reading but discards until the
/// session ends or it is rerouted). Never holds the zones lock when called.
fn detach_driver(&self, driver: ZoneDriver) {
    match driver {
        ZoneDriver::Url(pipeline) => pipeline.shutdown(),
        ZoneDriver::Airplay(source) => {
            let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
            if let Some(s) = sources.get_mut(&source) {
                s.routed = false;
                s.sink = None;
            }
        }
    }
}

/// An AirPlay session started on `source` (its FIFO opened). Make it the driver
/// of its dest_zone, last-wins over any URL/other source there. No sink is
/// resolved here — `sink_for_source` resolves lazily on the first chunk so an
/// idle receiver never holds a snapserver slot / open device.
pub fn session_began(&self, source: &str) {
    let dest = {
        let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
        let Some(s) = sources.get_mut(source) else { return };
        s.active = true;
        s.routed = true;
        s.sink = None;
        s.dest_zone.clone()
    };

    // Detach whatever drives dest now, then install this source. Snapshot+release
    // around any blocking shutdown.
    let prev = {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        if let Some(z) = zones.get_mut(&dest) {
            let prev = z.current.take();
            z.current = Some(ZoneDriver::Airplay(source.to_string()));
            prev
        } else {
            None
        }
    };
    if let Some(prev) = prev {
        self.detach_driver(prev);
    }

    self.notify_sources_changed();
    self.notify_outputs_changed(); // zone "playing" state changed
}

/// Where should `source` write right now? `None` if it has no active session or
/// has been detached (unrouted). Resolves and caches the dest_zone's sink on the
/// first call of a session (lock released around the blocking `zone_sink`).
pub fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>> {
    // Fast path: cached, or clearly not routed.
    let (dest, outputs) = {
        let sources = self.sources.lock().expect("engine sources mutex poisoned");
        let s = sources.get(source)?;
        if !s.active || !s.routed {
            return None;
        }
        if let Some(sink) = &s.sink {
            return Some(Arc::clone(sink));
        }
        let dest = s.dest_zone.clone();
        let outputs = {
            let zones = self.zones.lock().expect("engine zones mutex poisoned");
            zones.get(&dest).map(|z| z.outputs.clone())?
        };
        (dest, outputs)
    };

    // Resolve off the locks (zone_sink can spawn snapserver / open cpal).
    let sink = self.zone_sink(&dest, &outputs).ok()?;

    // Re-check still routed, then cache.
    let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
    let s = sources.get_mut(source)?;
    if !s.active || !s.routed {
        return None;
    }
    s.sink = Some(Arc::clone(&sink));
    Some(sink)
}

/// An AirPlay session ended (FIFO EOF). Clear the source's session state and, if
/// it still drives its dest_zone, clear that driver and free any Snapcast slot.
pub fn session_ended(&self, source: &str) {
    let dest = {
        let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
        let Some(s) = sources.get_mut(source) else { return };
        s.active = false;
        s.routed = false;
        s.sink = None;
        s.dest_zone.clone()
    };

    let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
    if let Some(z) = zones.get_mut(&dest) {
        if matches!(&z.current, Some(ZoneDriver::Airplay(s)) if s == source) {
            z.current = None;
        }
    }
    drop(zones);
    self.snapcast.release_zone(&dest);

    self.notify_sources_changed();
    self.notify_outputs_changed();
}

/// Active AirPlay sessions, for the `sources` push.
pub fn list_sources(&self) -> Vec<SourceView> {
    let sources = self.sources.lock().expect("engine sources mutex poisoned");
    sources
        .iter()
        .filter(|(_, s)| s.active)
        .map(|(id, s)| SourceView {
            source: id.clone(),
            name: s.name.clone(),
            dest_zone: s.dest_zone.clone(),
            routed: s.routed,
        })
        .collect()
}
```

Add the `SOURCES_CHANGED` broadcast next to `OUTPUTS_CHANGED` in the `lazy_static!` block:

```rust
/// Broadcast tick fired whenever the set/state of active AirPlay sources changes
/// (session begin/end, route/detach). Per-client connections re-push `sources`.
pub static ref SOURCES_CHANGED: broadcast::Sender<()> = broadcast::channel(16).0;
```

And the notifier:

```rust
fn notify_sources_changed(&self) {
    let _ = SOURCES_CHANGED.send(());
}
```

Add the **test-only helpers** behind `#[cfg(test)]` at the bottom of `impl Engine` (or in the tests module via a small `impl`):

```rust
#[cfg(test)]
impl Engine {
    /// Register an idle source (the device-free half of what reconcile installs).
    fn add_idle_source(&self, id: &str, name: &str) {
        self.sources.lock().unwrap().insert(
            id.to_string(),
            SourceState {
                name: name.to_string(),
                dest_zone: id.to_string(),
                active: false,
                routed: false,
                sink: None,
            },
        );
    }
    fn zone_has_airplay_driver(&self, zone: &str) -> bool {
        let zones = self.zones.lock().unwrap();
        matches!(zones.get(zone).and_then(|z| z.current.as_ref()), Some(ZoneDriver::Airplay(_)))
    }
    /// Install a URL driver via the same detach path play() uses (device-free:
    /// caller supplies a dummy pipeline).
    fn install_url_driver_for_test(&self, zone: &str, pipeline: Pipeline) {
        let prev = {
            let mut zones = self.zones.lock().unwrap();
            let z = zones.get_mut(zone).unwrap();
            let prev = z.current.take();
            z.current = Some(ZoneDriver::Url(pipeline));
            prev
        };
        if let Some(prev) = prev { self.detach_driver(prev); }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin audioshare_device audio::engine`
Expected: PASS (new session tests + all existing engine tests, which still compile against `Option<ZoneDriver>`).

- [ ] **Step 5: Commit**

```bash
git add src/audio/engine.rs
git commit -m "AirPlay slice 2: engine source model (ZoneDriver, source registry, session hooks)"
```

---

### Task 4: `ShairportManager` reconciliation against the zone set (pure, fake-backed)

A manager owns one receiver per zone and **reconciles** against the desired zone set. Put the spawn/kill/rename behind a `ReceiverFactory`/`ZoneReceiver` seam so the diff logic is tested with a fake — no `shairport-sync`, no threads.

**Files:**
- Create: `src/audio/airplay_manager.rs`
- Modify: `src/audio/mod.rs` (add `pub mod airplay_manager;` after `pub mod airplay;`)
- Test: inline `#[cfg(test)]` in `src/audio/airplay_manager.rs`

**Interfaces:**
- Produces:
  ```rust
  pub trait ZoneReceiver: Send + Sync { fn rename(&self, new_name: &str) -> Result<(), String>; }
  pub trait ReceiverFactory: Send + Sync {
      fn create(&self, zone: &str, name: &str, slot: usize) -> Result<Box<dyn ZoneReceiver>, String>;
  }
  pub struct ShairportManager { /* factory + receivers + slot pool */ }
  impl ShairportManager {
      pub fn new(factory: Box<dyn ReceiverFactory>) -> Self;
      pub fn reconcile(&self, desired: &[(String, String)]); // (zone_id, name)
  }
  ```
- Slot assignment: each zone gets a stable slot (lowest free index) on first spawn; freed on kill so the port/device-id pool stays bounded.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Log { events: Mutex<Vec<String>> }

    struct FakeReceiver { zone: String, slot: usize, log: Arc<Log> }
    impl ZoneReceiver for FakeReceiver {
        fn rename(&self, new_name: &str) -> Result<(), String> {
            self.log.events.lock().unwrap().push(format!("rename {} -> {}", self.zone, new_name));
            Ok(())
        }
    }
    impl Drop for FakeReceiver {
        fn drop(&mut self) {
            self.log.events.lock().unwrap().push(format!("kill {} slot{}", self.zone, self.slot));
        }
    }

    struct FakeFactory { log: Arc<Log> }
    impl ReceiverFactory for FakeFactory {
        fn create(&self, zone: &str, name: &str, slot: usize) -> Result<Box<dyn ZoneReceiver>, String> {
            self.log.events.lock().unwrap().push(format!("create {} '{}' slot{}", zone, name, slot));
            Ok(Box::new(FakeReceiver { zone: zone.to_string(), slot, log: Arc::clone(&self.log) }))
        }
    }

    fn drain(log: &Arc<Log>) -> Vec<String> { std::mem::take(&mut *log.events.lock().unwrap()) }

    #[test]
    fn reconcile_spawns_new_zones_and_assigns_slots() {
        let log = Arc::new(Log::default());
        let mgr = ShairportManager::new(Box::new(FakeFactory { log: Arc::clone(&log) }));

        mgr.reconcile(&[("default".into(), "Hub".into()), ("d1".into(), "Kitchen".into())]);
        let mut got = drain(&log);
        got.sort();
        assert_eq!(got, vec![
            "create d1 'Kitchen' slot1".to_string(),
            "create default 'Hub' slot0".to_string(),
        ]);

        // Reconciling the same set is a no-op (idempotent).
        mgr.reconcile(&[("default".into(), "Hub".into()), ("d1".into(), "Kitchen".into())]);
        assert!(drain(&log).is_empty());
    }

    #[test]
    fn reconcile_kills_removed_and_renames_changed() {
        let log = Arc::new(Log::default());
        let mgr = ShairportManager::new(Box::new(FakeFactory { log: Arc::clone(&log) }));
        mgr.reconcile(&[("d1".into(), "Kitchen".into())]);
        drain(&log);

        // Rename d1, remove nothing.
        mgr.reconcile(&[("d1".into(), "Cucina".into())]);
        assert_eq!(drain(&log), vec!["rename d1 -> Cucina".to_string()]);

        // Remove d1 entirely -> its receiver is dropped (kill).
        mgr.reconcile(&[]);
        assert_eq!(drain(&log), vec!["kill d1 slot0".to_string()]);

        // A new zone reuses the now-free slot 0.
        mgr.reconcile(&[("d2".into(), "Bath".into())]);
        assert_eq!(drain(&log), vec!["create d2 'Bath' slot0".to_string()]);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin audioshare_device audio::airplay_manager`
Expected: FAIL — module/types not found.

- [ ] **Step 3: Implement**

```rust
//! AirPlay receiver supervision (Phase 4, Slice 2).
//!
//! [`ShairportManager`] keeps one classic `shairport-sync` receiver per zone,
//! reconciling against the live zone set the same way the Snapcast reconciler
//! converges snapserver: spawn receivers for new zones, kill them for removed
//! zones, restart-renamed for renamed zones. Spawning is behind the
//! [`ReceiverFactory`]/[`ZoneReceiver`] seam so the diff is unit-tested with a
//! fake — no `shairport-sync`, no audio. The production factory lives in
//! `audio::airplay_factory`.

use std::collections::HashMap;
use std::sync::Mutex;

/// A live AirPlay receiver for one zone (a supervised `shairport-sync` + its PCM
/// pump thread). Dropping it tears both down.
pub trait ZoneReceiver: Send + Sync {
    /// Restart the receiver advertising `new_name` (the mDNS/AirPlay name).
    fn rename(&self, new_name: &str) -> Result<(), String>;
}

/// Creates [`ZoneReceiver`]s. The production impl spawns `shairport-sync`; tests
/// use a fake that records calls.
pub trait ReceiverFactory: Send + Sync {
    fn create(&self, zone: &str, name: &str, slot: usize) -> Result<Box<dyn ZoneReceiver>, String>;
}

struct Managed {
    name: String,
    slot: usize,
    receiver: Box<dyn ZoneReceiver>,
}

/// Owns the per-zone receivers and converges them to the desired zone set.
pub struct ShairportManager {
    factory: Box<dyn ReceiverFactory>,
    receivers: Mutex<HashMap<String, Managed>>, // keyed by zone id
}

impl ShairportManager {
    pub fn new(factory: Box<dyn ReceiverFactory>) -> Self {
        Self { factory, receivers: Mutex::new(HashMap::new()) }
    }

    /// Converge receivers to `desired` (zone_id, name): spawn missing, kill
    /// removed, rename changed. Idempotent. Slot assignment is lowest-free so the
    /// port/device-id pool stays small and stable.
    pub fn reconcile(&self, desired: &[(String, String)]) {
        let mut receivers = self.receivers.lock().expect("airplay receivers mutex poisoned");

        // Kill receivers whose zone is gone (drop runs the receiver's teardown).
        let desired_ids: std::collections::HashSet<&str> =
            desired.iter().map(|(id, _)| id.as_str()).collect();
        receivers.retain(|id, _| desired_ids.contains(id.as_str()));

        for (id, name) in desired {
            match receivers.get_mut(id) {
                Some(m) => {
                    if &m.name != name {
                        if m.receiver.rename(name).is_ok() {
                            m.name = name.clone();
                        }
                    }
                }
                None => {
                    let slot = lowest_free_slot(&receivers);
                    match self.factory.create(id, name, slot) {
                        Ok(receiver) => {
                            receivers.insert(id.clone(), Managed { name: name.clone(), slot, receiver });
                        }
                        Err(e) => {
                            eprintln!("airplay: failed to start receiver for zone {id}: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// Lowest slot index not currently in use by an existing receiver.
fn lowest_free_slot(receivers: &HashMap<String, Managed>) -> usize {
    let used: std::collections::HashSet<usize> = receivers.values().map(|m| m.slot).collect();
    (0..).find(|s| !used.contains(s)).unwrap()
}
```

Add to `src/audio/mod.rs` after `pub mod airplay;`:

```rust
pub mod airplay_manager;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin audioshare_device audio::airplay_manager`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay_manager.rs src/audio/mod.rs
git commit -m "AirPlay slice 2: ShairportManager reconciles receivers against the zone set"
```

---

### Task 5: Production receiver factory (binary + pump thread → engine)

The factory that actually spawns `shairport-sync` (Task 1) and a `run_receiver` pump thread (Task 2) whose resolver calls into the engine (Task 3) via a `SessionSink` seam. This is the integration glue — no device-free unit test (it needs the binary + a real session); a construction-only test guards the wiring.

**Files:**
- Create: `src/audio/airplay_factory.rs`
- Modify: `src/audio/mod.rs` (add `pub mod airplay_factory;`)
- Modify: `src/audio/engine.rs` (add `impl SessionSink for &'static Engine`)
- Test: inline `#[cfg(test)]` in `src/audio/airplay_factory.rs`

**Interfaces:**
- Consumes: `airplay::{ShairportSupervisor, run_receiver, fifo_path}`, `airplay_manager::{ReceiverFactory, ZoneReceiver}`, `engine::SessionSink`.
- Produces:
  ```rust
  pub trait SessionSink: Send + Sync {   // defined in engine.rs
      fn session_began(&self, source: &str);
      fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>>;
      fn session_ended(&self, source: &str);
  }
  pub struct ShairportReceiverFactory { sessions: Arc<dyn SessionSink> }
  impl ShairportReceiverFactory { pub fn new(sessions: Arc<dyn SessionSink>) -> Self; }
  impl ReceiverFactory for ShairportReceiverFactory { /* create spawns supervisor + pump thread */ }
  ```

- [ ] **Step 1: Define `SessionSink` in `engine.rs` and implement it for the engine**

Add to `src/audio/engine.rs` (near the top, after the type aliases):

```rust
/// The slice of the engine an AirPlay pump thread needs: bracket a session and
/// ask, per chunk, where the source's audio goes right now. A trait so the
/// production receiver factory can be wired without a hard dependency cycle and
/// the engine's session logic stays unit-testable in isolation.
pub trait SessionSink: Send + Sync {
    fn session_began(&self, source: &str);
    fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>>;
    fn session_ended(&self, source: &str);
}

impl SessionSink for &'static Engine {
    fn session_began(&self, source: &str) { Engine::session_began(self, source); }
    fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>> {
        Engine::sink_for_source(self, source)
    }
    fn session_ended(&self, source: &str) { Engine::session_ended(self, source); }
}
```

(`ENGINE` is a `lazy_static` with `'static` lifetime, so `&*ENGINE` coerces to `&'static Engine`.)

- [ ] **Step 2: Write the construction test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sink::AudioSink;
    use std::sync::Arc;

    struct NoSessions;
    impl crate::audio::engine::SessionSink for NoSessions {
        fn session_began(&self, _s: &str) {}
        fn sink_for_source(&self, _s: &str) -> Option<Arc<dyn AudioSink>> { None }
        fn session_ended(&self, _s: &str) {}
    }

    #[test]
    fn factory_constructs_without_spawning() {
        // Construction must not touch shairport-sync or any device.
        let _factory = ShairportReceiverFactory::new(Arc::new(NoSessions));
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --bin audioshare_device audio::airplay_factory`
Expected: FAIL — module/types not found.

- [ ] **Step 4: Implement**

```rust
//! Production AirPlay receiver factory (Phase 4, Slice 2).
//!
//! Spawns a classic `shairport-sync` per zone ([`airplay::ShairportSupervisor`])
//! plus a [`airplay::run_receiver`] pump thread whose per-chunk resolver routes
//! through the engine ([`SessionSink`]). The session is bracketed by the audio
//! FIFO (open = start, EOF = end). Renaming restarts the supervisor with the new
//! AirPlay name. Demo-gated end-to-end: see the bring-up notes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::audio::airplay::{self, ShairportSupervisor};
use crate::audio::airplay_manager::{ReceiverFactory, ZoneReceiver};
use crate::audio::engine::SessionSink;

/// Builds real `shairport-sync`-backed receivers wired into the engine.
pub struct ShairportReceiverFactory {
    sessions: Arc<dyn SessionSink>,
}

impl ShairportReceiverFactory {
    pub fn new(sessions: Arc<dyn SessionSink>) -> Self {
        Self { sessions }
    }
}

impl ReceiverFactory for ShairportReceiverFactory {
    fn create(&self, zone: &str, name: &str, slot: usize) -> Result<Box<dyn ZoneReceiver>, String> {
        let supervisor = Mutex::new(Some(ShairportSupervisor::spawn_for_slot(name, slot)?));
        let fifo = airplay::fifo_path(slot);
        let stop = Arc::new(AtomicBool::new(false));

        // The pump thread: bracket sessions via the FIFO, route per chunk.
        let pump = {
            let sessions = Arc::clone(&self.sessions);
            let source = zone.to_string();
            let stop = Arc::clone(&stop);
            let fifo = fifo.clone();
            thread::Builder::new()
                .name(format!("airplay-pump-{slot}"))
                .spawn(move || {
                    let began = || sessions.session_began(&source);
                    let resolve = || sessions.sink_for_source(&source);
                    let ended = || sessions.session_ended(&source);
                    if let Err(e) = airplay::run_receiver(&fifo, &stop, began, resolve, ended) {
                        eprintln!("airplay pump for {source} ended: {e}");
                    }
                })
                .map_err(|e| format!("failed to spawn airplay pump thread: {e}"))?
        };

        Ok(Box::new(ShairportReceiver {
            name: Mutex::new(name.to_string()),
            slot,
            supervisor,
            stop,
            pump: Mutex::new(Some(pump)),
            fifo,
        }))
    }
}

/// One live receiver: the supervised process + its pump thread. Drop tears both
/// down (stop the pump, kill shairport via the supervisor's own Drop).
struct ShairportReceiver {
    name: Mutex<String>,
    slot: usize,
    supervisor: Mutex<Option<ShairportSupervisor>>,
    stop: Arc<AtomicBool>,
    pump: Mutex<Option<JoinHandle<()>>>,
    fifo: PathBuf,
}

impl ZoneReceiver for ShairportReceiver {
    fn rename(&self, new_name: &str) -> Result<(), String> {
        // Restart the supervisor with the new AirPlay/mDNS name; the pump thread
        // (and its FIFO) are unaffected.
        let next = ShairportSupervisor::spawn_for_slot(new_name, self.slot)?;
        *self.supervisor.lock().expect("supervisor mutex poisoned") = Some(next);
        *self.name.lock().expect("name mutex poisoned") = new_name.to_string();
        Ok(())
    }
}

impl Drop for ShairportReceiver {
    fn drop(&mut self) {
        // Stop the pump first so it isn't blocked on a FIFO that no writer will
        // ever open again; then drop the supervisor (kills shairport).
        self.stop.store(true, Ordering::Relaxed);
        // The pump may be parked in a blocking FIFO open with no writer. Drop the
        // supervisor to kill shairport, then nudge the open by briefly opening the
        // write end ourselves so the parked open returns and the thread observes
        // `stop`. (Best-effort; a half-open FIFO read returns EOF on our close.)
        *self.supervisor.lock().expect("supervisor mutex poisoned") = None;
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&self.fifo) {
            use std::io::Write;
            let _ = f.write_all(&[]);
        }
        if let Some(handle) = self.pump.lock().expect("pump mutex poisoned").take() {
            let _ = handle.join();
        }
    }
}
```

Add to `src/audio/mod.rs` after `pub mod airplay_manager;`:

```rust
pub mod airplay_factory;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --bin audioshare_device audio::airplay_factory`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/audio/airplay_factory.rs src/audio/mod.rs src/audio/engine.rs
git commit -m "AirPlay slice 2: production shairport receiver factory (pump thread -> engine)"
```

---

### Task 6: Wire the manager into the engine + reconcile on zone changes

The engine gains an optional `ShairportManager` (enabled at startup) and **reconciles it on every zone-topology change**, while also keeping the logical `sources` registry in step. Disabled in tests → device-free; enabled in production via `enable_airplay`.

**Files:**
- Modify: `src/audio/engine.rs`
- Test: inline `#[cfg(test)]` in `src/audio/engine.rs`

**Interfaces:**
- Consumes: `airplay_manager::{ShairportManager, ReceiverFactory}`.
- Produces:
  ```rust
  pub fn enable_airplay(&self, factory: Box<dyn ReceiverFactory>);
  // private: fn reconcile_airplay(&self);  // updates sources map + manager
  ```
- `reconcile_airplay` is called from `create_zone`, `delete_zone`, `rename_zone`, `add_dongle_output` — every place that already mutates the zone set.

- [ ] **Step 1: Write the failing test (fake factory, device-free)**

```rust
#[test]
fn enabling_airplay_reconciles_existing_and_new_zones() {
    use crate::audio::airplay_manager::{ReceiverFactory, ZoneReceiver};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Spy { created: Mutex<Vec<String>> }
    struct Recv;
    impl ZoneReceiver for Recv { fn rename(&self, _n: &str) -> Result<(), String> { Ok(()) } }
    struct SpyFactory { spy: Arc<Spy> }
    impl ReceiverFactory for SpyFactory {
        fn create(&self, zone: &str, _name: &str, _slot: usize) -> Result<Box<dyn ZoneReceiver>, String> {
            self.spy.created.lock().unwrap().push(zone.to_string());
            Ok(Box::new(Recv))
        }
    }

    let engine = Engine::new();
    let spy = Arc::new(Spy::default());
    engine.enable_airplay(Box::new(SpyFactory { spy: Arc::clone(&spy) }));

    // Enabling spawns a receiver for the pre-existing default zone, and registers
    // an idle source for it.
    {
        let created = spy.created.lock().unwrap();
        assert!(created.contains(&"default".to_string()), "default zone got a receiver");
    }
    assert!(engine.has_source("default")); // test helper from Task 3 area

    // A new dongle zone reconciles a new receiver + idle source.
    engine.add_dongle_output("d1", "Kitchen");
    {
        let created = spy.created.lock().unwrap();
        assert!(created.contains(&"d1".to_string()), "dongle zone got a receiver");
    }
    assert!(engine.has_source("d1"));
}
```

Add the `has_source` test helper to the `#[cfg(test)] impl Engine` block from Task 3:

```rust
fn has_source(&self, id: &str) -> bool {
    self.sources.lock().unwrap().contains_key(id)
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin audioshare_device audio::engine::tests::enabling_airplay`
Expected: FAIL — `enable_airplay` not found.

- [ ] **Step 3: Implement**

Add the field to `Engine` and init in `new()`:

```rust
// in struct Engine:
airplay: Mutex<Option<ShairportManager>>,

// in Engine::new() struct literal:
airplay: Mutex::new(None),
```

Add the import at the top of `engine.rs`:

```rust
use crate::audio::airplay_manager::{ReceiverFactory, ShairportManager};
```

Add the methods:

```rust
/// Turn on AirPlay receiving: install the receiver manager and reconcile it (and
/// the logical source registry) against the current zone set, spawning a
/// receiver per zone. Idempotent-ish: a second call replaces the manager.
pub fn enable_airplay(&self, factory: Box<dyn ReceiverFactory>) {
    *self.airplay.lock().expect("engine airplay mutex poisoned") = Some(ShairportManager::new(factory));
    self.reconcile_airplay();
}

/// Converge the logical source registry and (if AirPlay is enabled) the receiver
/// manager to the current zone set. Called after any zone-topology change.
fn reconcile_airplay(&self) {
    // Desired = every zone (id, name).
    let desired: Vec<(String, String)> = {
        let zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones.iter().map(|(id, z)| (id.clone(), z.name.clone())).collect()
    };

    // Keep the logical source registry in step: an idle source per zone; drop
    // sources whose zone is gone. Preserve session state for surviving zones.
    {
        let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
        let desired_ids: std::collections::HashSet<&str> =
            desired.iter().map(|(id, _)| id.as_str()).collect();
        sources.retain(|id, _| desired_ids.contains(id.as_str()));
        for (id, name) in &desired {
            sources
                .entry(id.clone())
                .and_modify(|s| s.name = name.clone())
                .or_insert_with(|| SourceState {
                    name: name.clone(),
                    dest_zone: id.clone(),
                    active: false,
                    routed: false,
                    sink: None,
                });
        }
    }

    if let Some(mgr) = self.airplay.lock().expect("engine airplay mutex poisoned").as_ref() {
        mgr.reconcile(&desired);
    }
}
```

Call `self.reconcile_airplay();` at the end of each zone-topology mutator — **after** the existing `notify_outputs_changed()` in: `create_zone`, `delete_zone`, `rename_zone`, and `add_dongle_output`. For example, in `add_dongle_output` add after the zone insert:

```rust
drop(zones); // release before reconcile (which re-locks zones)
self.reconcile_airplay();
```

(`create_zone`/`delete_zone`/`rename_zone` already drop the `zones` lock before returning; insert the `reconcile_airplay()` call after `notify_outputs_changed()`.)

> Lock note: `reconcile_airplay` re-locks `zones` and `sources`; ensure callers are not holding either when they call it. `add_dongle_output` holds `zones` — drop it first as shown.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin audioshare_device audio::engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/audio/engine.rs
git commit -m "AirPlay slice 2: engine owns the receiver manager, reconciles on zone changes"
```

---

### Task 7: Wire protocol — `list_sources` task + `sources` push

Expose active AirPlay sessions to the app: a `sources` push (on connect, on `SOURCES_CHANGED`) and a `list_sources` pull. Mirrors the existing `outputs`/`zones` plumbing.

**Files:**
- Modify: `src/server/commands.rs` (add `Task::ListSources` parse/name)
- Modify: `src/server/connection.rs` (subscribe `SOURCES_CHANGED`, `send_sources`, handle `list_sources`)
- Test: inline `#[cfg(test)]` in `src/server/commands.rs`

**Interfaces:**
- Consumes: `engine::{ENGINE, SOURCES_CHANGED, SourceView}`.
- Produces: `Task::ListSources` (wire name `list_sources`); `Connection::send_sources`.
- Wire (frozen in CLAUDE.md, Task 8):
  ```json
  { "status": "ok", "task": "sources",
    "data": { "sources": [ { "source": "<home-zone-id>", "name": "Kitchen",
                             "dest_zone": "<zone-id>", "active": true, "routed": true } ] } }
  ```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn parses_list_sources_task() {
    assert_eq!(Task::parse("list_sources"), Task::ListSources);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin audioshare_device server::commands::tests::parses_list_sources`
Expected: FAIL — `Task::ListSources` not found.

- [ ] **Step 3: Implement the task variant**

In `src/server/commands.rs`, add `ListSources` to the `Task` enum (next to `ListOutputs`), to `parse` (`"list_sources" => Task::ListSources,`), and to `name` (`Task::ListSources => "list_sources",`). It is handled in `connection.rs` (it pushes), so `dispatch` needs no arm — but the catch-all stub would mishandle it. Since `handle_task` intercepts it before `dispatch` (Step 4), no `dispatch` arm is required; leave `dispatch` as-is.

- [ ] **Step 4: Wire the push into the connection**

In `src/server/connection.rs`:

Add `SOURCES_CHANGED` to the import:

```rust
use crate::audio::engine::{ENGINE, OUTPUTS_CHANGED, SOURCES_CHANGED};
```

In `listen`, subscribe and push on connect (after the existing `send_zones`):

```rust
let mut sources_changed = SOURCES_CHANGED.subscribe();
if self.send_sources().await.is_err() {
    return Ok(true);
}
```

Add a branch to the `tokio::select!` for source changes (alongside the `outputs_changed.recv()` arm):

```rust
_ = sources_changed.recv() => {
    if self.send_sources().await.is_err() {
        return Ok(true);
    }
    continue;
}
```

Intercept `list_sources` in `handle_task` (next to `list_outputs`/`list_zones`):

```rust
Some("list_sources") => return self.send_sources().await,
```

Add `send_sources`:

```rust
/// Push currently-active AirPlay sessions to this client as an encrypted
/// `{"status":"ok","task":"sources","data":{"sources":[...]}}` message. Sent on
/// connect, on every SOURCES_CHANGED tick, and on a `list_sources` request.
async fn send_sources(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sources: Vec<serde_json::Value> = ENGINE
        .list_sources()
        .into_iter()
        .map(|s| json!({
            "source": s.source, "name": s.name, "dest_zone": s.dest_zone,
            "active": true, "routed": s.routed
        }))
        .collect();
    let response = TaskResponse::accepted("sources", Some(json!({ "sources": sources })));
    self.send_encrypted(&response.to_json()).await
}
```

- [ ] **Step 5: Run tests + build**

Run: `cargo test --bin audioshare_device server::commands` and `cargo build`
Expected: PASS / clean build.

- [ ] **Step 6: Commit**

```bash
git add src/server/commands.rs src/server/connection.rs
git commit -m "AirPlay slice 2: sources push + list_sources task"
```

---

### Task 8: Startup wiring, docs, and full verification

Turn AirPlay on at process start, freeze the wire additions in `CLAUDE.md`, record the design deviation in the spec, mark the slice done, and run the whole suite.

**Files:**
- Modify: `src/main.rs` (call `ENGINE.enable_airplay(...)` at startup)
- Modify: `CLAUDE.md` (protocol section: `sources` push, `list_sources`, current state note)
- Modify: `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md` (Slice 2 status + audio-FIFO deviation)
- Modify: `docs/multi-room-plan.md` (bring-up note: disable distro `shairport-sync.service`)

- [ ] **Step 1: Enable AirPlay at startup**

In `src/main.rs`, after `let server = Arc::new(server::server::Server::new());` and before `server.start().await;`, enable AirPlay against the global engine:

```rust
use audio::engine::ENGINE;
use audio::airplay_factory::ShairportReceiverFactory;
use std::sync::Arc as StdArc;

// Turn on AirPlay receiving: one classic shairport-sync per zone, routed through
// the engine. If shairport-sync isn't installed, individual spawns just log and
// are skipped (the rest of the server still runs).
let sessions: StdArc<dyn audio::engine::SessionSink> = StdArc::new(&*ENGINE);
ENGINE.enable_airplay(Box::new(ShairportReceiverFactory::new(sessions)));
```

> `&*ENGINE` is `&'static Engine` (ENGINE is `lazy_static`), matching `impl SessionSink for &'static Engine` from Task 5. Verify it compiles; if the coercion needs a hand, bind `let engine_ref: &'static Engine = &ENGINE;` first.

- [ ] **Step 2: Build the whole workspace**

Run: `cargo build --workspace`
Expected: clean build (no warnings introduced; treat new dead-code warnings as TODO to resolve).

- [ ] **Step 3: Run the full test suite**

Run: `cargo test --workspace`
Expected: all tests pass (the 63 existing device-server tests + the new airplay/manager/engine/commands tests; protocol + dongle_agent crates unchanged).

- [ ] **Step 4: Freeze the wire protocol in `CLAUDE.md`**

In the "Cross-project wire protocol" section: add `list_sources` to the recognized-tasks list; document the `sources` push (server → iOS, marked "hub-side shipped, iOS mirror pending own spec") with the JSON shape from Task 7; note `SOURCES_CHANGED` alongside `OUTPUTS_CHANGED`. In the current-state paragraph, append a sentence: AirPlay Slice 2 is in — per-zone classic `shairport-sync` receivers reconciled against the zone set, audio-FIFO-bracketed sessions routed through `zone_sink()` (local or dongle), `sources` push + `list_sources`; track metadata/art (Slice 3) and reroute (Slice 4) are not yet built.

- [ ] **Step 5: Record the design deviation + slice status in the spec**

In `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md`, under the Slicing section's Slice 2 bullet, add a note: *"As built, sessions are bracketed by the audio FIFO (open/EOF), not the metadata pipe — the metadata pipe is introduced in Slice 3 (track info). This avoids interrupting a blocked PCM reader and makes routing per-chunk (reroute-ready)."* Mark Slice 2 done.

- [ ] **Step 6: Add the bring-up gotcha to the multi-room plan**

In `docs/multi-room-plan.md` "Bring-up notes", add: *"AirPlay: `apt install shairport-sync` enables a systemd `shairport-sync.service` that auto-starts an instance grabbing an AirPlay name + ports — the analog of the `snapserver.service` collision. Disable it: `sudo systemctl disable --now shairport-sync` so the hub owns its supervised per-zone instances. The classic (not AirPlay-2) build is required for multiple instances per host."*

- [ ] **Step 7: Commit**

```bash
git add src/main.rs CLAUDE.md docs/superpowers/specs/2026-06-23-airplay-receiver-design.md docs/multi-room-plan.md
git commit -m "AirPlay slice 2: enable receivers at startup; freeze wire protocol + docs"
```

- [ ] **Step 8: Demo-gated end-to-end verification (on the Pi, manual)**

Not CI. On the Pi hub with a dongle attached and the distro `shairport-sync.service` disabled:
1. `cargo run` (or deploy via `./to_pi.sh device` + `./run.sh device`).
2. On an iPhone, open the AirPlay menu — confirm "Hub" and each dongle zone (e.g. "Kitchen") appear as separate AirPlay targets.
3. AirPlay to "Hub" → sound from the hub speaker; the app's `sources` shows Hub active.
4. AirPlay to "Kitchen" → sound from the Kitchen dongle (via Snapcast); the app shows Kitchen active.
5. Stop on the phone → the source goes inactive; `sources` empties.

---

## Self-Review

**Spec coverage (design doc Slice 2 bullet):**
- Per-zone receivers reconciled against the zone set incl. dongle auto-zones → Tasks 4 + 6 (reconcile called from `add_dongle_output`).
- Session begin/end → Task 2 (audio-FIFO bracket; **deviation** from metadata pipe, recorded Task 8 Step 5, justified in Global Constraints).
- Each source feeds its home zone through `zone_sink()` (dongle zones via snapserver) → Task 3 `sink_for_source` → existing `zone_sink`.
- One-driver-per-zone conflict handling → Task 3 `ZoneDriver` + `detach_driver` + `play`/`stop` updates.
- `sources` push, `SOURCES_CHANGED`, `list_sources` → Tasks 3 (broadcast + `list_sources` engine method) + 7 (wire).

**Type consistency:** `ZoneDriver`/`SourceState`/`SourceView` defined in Task 3 and used consistently in 5/6/7. `ReceiverFactory`/`ZoneReceiver` defined Task 4, implemented Task 5, consumed Task 6. `SessionSink` defined Task 5 (in `engine.rs`), implemented for `&'static Engine` (Task 5) and consumed by the factory (Task 5) + main (Task 8). `session_began`/`sink_for_source`/`session_ended`/`list_sources` signatures match across Tasks 3/5/7.

**Placeholder scan:** every code step shows complete code; no "TBD"/"handle errors"/"similar to". Test helpers (`add_idle_source`, `zone_has_airplay_driver`, `install_url_driver_for_test`, `has_source`) are all defined in Task 3/6.

**Known follow-ups (out of Slice 2 scope):** track metadata/art + the metadata pipe (Slice 3); `reroute` task + `dest_zone != home_zone` (Slice 4); `unknown_source` error code (lands with reroute); iOS now-playing/reroute UI (separate spec). The per-chunk resolver (Task 2/3) is deliberately reroute-ready.
