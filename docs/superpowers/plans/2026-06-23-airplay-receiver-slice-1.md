# AirPlay Receiver — Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the classic-AirPlay receive path end-to-end: an iPhone AirPlays to the hub and sound comes out the hub's local speaker.

**Architecture:** A supervised `shairport-sync` (classic AirPlay) process writes raw `s16le` 44100/2 PCM into a named FIFO via its `pipe` output backend. A reader thread reads the FIFO, converts `s16le`→`f32`, resamples/mixes to the local sink's format (reusing `decode.rs`'s `ResamplePipeline`/`mix_planar`), and writes into an `AudioSink`. This mirrors `audio/snapcast.rs` (sub-step 1 of the Snapcast work) in reverse — and, exactly like that sub-step, Slice 1 proves the path **standalone via a demo-gated test**; wiring into the live `Engine`/server is deferred to Slice 2 (the `ShairportManager`).

**Tech Stack:** Rust (edition 2021), `libc` (FIFO `mkfifo`), `rubato` (resampling, via `decode.rs`), `shairport-sync` (external, classic build), std `process`/`thread`/`fs`.

## Global Constraints

- Rust edition 2021; recent stable toolchain (≥1.85; 1.96 in use) — copied from spec/CLAUDE.md.
- AirPlay PCM is fixed **44100 Hz, 16-bit, 2 channels** (`s16le` interleaved). Copy these verbatim.
- `AudioSink::write` requires samples **already at the sink's `sample_rate()`/`channels()`** — the reader must resample/mix to the sink, never assume rates match.
- New code must add **device-free unit tests** (CI-safe, no audio hardware, no `shairport-sync`); the live audio path is **demo-gated** with `#[ignore]`, mirroring `audio::snapcast::tests::plays_to_snapcast_briefly`.
- V1 uses the **classic (non-AirPlay-2) build** of `shairport-sync` — the AirPlay-2 build can't run multiple instances per host (this matters from Slice 2 on; document it now).
- Mirror existing module style: module-level `//!` doc, `const` for fixed values, `Drop`-based process cleanup, pure helpers split out for testing.

---

### Task 1: Module scaffold + `s16le`→planar-`f32` conversion

Create the AirPlay module and the inverse of `snapcast.rs`'s `to_i16le`: turn interleaved little-endian `s16` bytes into planar `f32` channels.

**Files:**
- Create: `src/audio/airplay.rs`
- Modify: `src/audio/mod.rs:8` (add `pub mod airplay;` after `pub mod snapcast_router;`)
- Test: inline `#[cfg(test)]` in `src/audio/airplay.rs`

**Interfaces:**
- Produces: `pub(crate) fn i16le_to_planar_f32(bytes: &[u8], channels: usize) -> Vec<Vec<f32>>` — `bytes.len()` must be a whole number of frames (`channels * 2` bytes each); any trailing partial frame is ignored. Returns `channels` planar `Vec<f32>` in `[-1.0, 1.0]`.
- Produces: `pub const AIRPLAY_SAMPLE_RATE: u32 = 44_100;` and `pub const AIRPLAY_CHANNELS: usize = 2;`

- [ ] **Step 1: Add the module declaration**

In `src/audio/mod.rs`, add the line so it reads:

```rust
pub mod airplay;
pub mod decode;
pub mod engine;
pub mod output;
pub mod registry;
pub mod sink;
pub mod snapcast;
pub mod snapcast_control;
pub mod snapcast_router;
```

- [ ] **Step 2: Write the failing test**

Create `src/audio/airplay.rs` with the doc header, constants, and a test (no implementation yet):

```rust
//! AirPlay receive path (Phase 4, Slice 1).
//!
//! The hub's first **receiver** source: audio is pushed *to* us by the phone's
//! own app, rather than fetched by the hub. A supervised `shairport-sync`
//! (classic AirPlay) writes raw `s16le` 44100/2 PCM into a named FIFO via its
//! `pipe` backend; [`pump_fifo_to_sink`] reads that FIFO, converts to `f32`,
//! resamples/mixes to the sink's format (reusing [`crate::audio::decode`]'s
//! pipeline), and writes into an [`AudioSink`]. Snapcast stays untouched — an
//! AirPlay source resolves to a zone's sink through the same seam a URL does.
//!
//! This mirrors `audio::snapcast` in reverse. Slice 1 proves the path with a
//! demo-gated test; per-zone supervision + engine wiring is Slice 2.

/// AirPlay always delivers CD audio: 44.1 kHz, 16-bit, stereo.
pub const AIRPLAY_SAMPLE_RATE: u32 = 44_100;
pub const AIRPLAY_CHANNELS: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16le_to_planar_deinterleaves_and_scales() {
        // Two stereo frames: (L=i16::MAX, R=0), (L=-i16::MAX, R=i16::MIN+1).
        // i16::MAX -> ~1.0, 0 -> 0.0, -i16::MAX -> ~-1.0.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&i16::MAX.to_le_bytes()); // L0
        bytes.extend_from_slice(&0i16.to_le_bytes()); // R0
        bytes.extend_from_slice(&(-i16::MAX).to_le_bytes()); // L1
        bytes.extend_from_slice(&(-i16::MAX).to_le_bytes()); // R1

        let planar = i16le_to_planar_f32(&bytes, 2);
        assert_eq!(planar.len(), 2);
        assert_eq!(planar[0].len(), 2); // 2 frames per channel
        assert!((planar[0][0] - 1.0).abs() < 1e-3); // L0 ~ +1.0
        assert!(planar[1][0].abs() < 1e-6); // R0 == 0.0
        assert!((planar[0][1] + 1.0).abs() < 1e-3); // L1 ~ -1.0
    }

    #[test]
    fn i16le_to_planar_ignores_trailing_partial_frame() {
        // 5 bytes = one whole stereo frame (4 bytes) + 1 stray byte.
        let bytes = [0u8, 0, 0, 0, 0];
        let planar = i16le_to_planar_f32(&bytes, 2);
        assert_eq!(planar[0].len(), 1);
        assert_eq!(planar[1].len(), 1);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib audio::airplay::tests::i16le_to_planar_deinterleaves_and_scales`
Expected: FAIL to compile — `cannot find function `i16le_to_planar_f32``.

- [ ] **Step 4: Write the minimal implementation**

Add above the `#[cfg(test)]` block:

```rust
/// Convert interleaved little-endian `s16` `bytes` into `channels` planar `f32`
/// channels in `[-1.0, 1.0]`. A trailing partial frame (fewer than
/// `channels * 2` bytes) is ignored — callers carry the remainder.
pub(crate) fn i16le_to_planar_f32(bytes: &[u8], channels: usize) -> Vec<Vec<f32>> {
    let frame_bytes = channels * 2;
    let frames = if frame_bytes == 0 { 0 } else { bytes.len() / frame_bytes };
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for f in 0..frames {
        for (ch, plane) in planar.iter_mut().enumerate() {
            let i = (f * channels + ch) * 2;
            let sample = i16::from_le_bytes([bytes[i], bytes[i + 1]]);
            plane.push(sample as f32 / i16::MAX as f32);
        }
    }
    planar
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib audio::airplay`
Expected: PASS (both tests).

- [ ] **Step 6: Commit**

```bash
git add src/audio/mod.rs src/audio/airplay.rs
git commit -m "AirPlay slice 1: airplay module + s16le->planar f32 conversion"
```

---

### Task 2: Expose `decode.rs` resample/mix helpers for reuse

The AirPlay reader must resample 44100→device-rate and mix 2→device-channels — the same work `decode.rs` already does. Make its `ResamplePipeline` and `mix_planar` reusable instead of duplicating them (DRY). Minimal change: widen visibility to `pub(crate)`; no behavior change.

**Files:**
- Modify: `src/audio/decode.rs` (visibility of `ResamplePipeline`, its `new`/`push_and_drain`, and `mix_planar`)

**Interfaces:**
- Produces: `pub(crate) struct ResamplePipeline` with `pub(crate) fn new(in_rate: u32, in_channels: usize, out_rate: u32, out_channels: usize) -> Result<Self, String>` and `pub(crate) fn push_and_drain(&mut self, mixed: Vec<Vec<f32>>, output: &dyn AudioSink)`.
- Produces: `pub(crate) fn mix_planar(input: &[Vec<f32>], out_channels: usize) -> Vec<Vec<f32>>`.

- [ ] **Step 1: Widen `ResamplePipeline` visibility**

In `src/audio/decode.rs`, change the struct declaration (currently `struct ResamplePipeline {`) to:

```rust
pub(crate) struct ResamplePipeline {
```

- [ ] **Step 2: Widen the method visibility**

In the `impl ResamplePipeline` block, change `fn new(` to `pub(crate) fn new(` and `fn push_and_drain(` to `pub(crate) fn push_and_drain(`.

- [ ] **Step 3: Widen `mix_planar` visibility**

Change `fn mix_planar(` to `pub(crate) fn mix_planar(`.

- [ ] **Step 4: Verify the whole crate still builds and tests pass**

Run: `cargo test --lib audio::decode`
Expected: PASS — existing decode tests unchanged (`mix_*`, `interleave_*`). No new warnings about unused visibility.

- [ ] **Step 5: Commit**

```bash
git add src/audio/decode.rs
git commit -m "AirPlay slice 1: expose decode ResamplePipeline + mix_planar for reuse"
```

---

### Task 3: `ShairportSupervisor` (spawn/restart/kill classic shairport-sync)

A thin supervisor mirroring `SnapserverSupervisor`: ensure the FIFO exists, write a minimal `shairport-sync` config, launch `shairport-sync -c <conf> -o pipe`, relaunch on exit, kill on drop. Config/arg building is pure and unit-tested; spawning is exercised by the demo gate (Task 5).

**Files:**
- Modify: `src/audio/airplay.rs`
- Test: inline `#[cfg(test)]` in `src/audio/airplay.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn fifo_path(index: usize) -> std::path::PathBuf` → `/tmp/audioshare-airplay-{index}.pcm`.
- Produces: `pub struct ShairportSupervisor` with `pub fn spawn(name: &str, port: u16, fifo: &std::path::Path) -> Result<Self, String>` and `pub fn spawn_with(binary: impl Into<String>, name: &str, port: u16, fifo: &std::path::Path) -> Result<Self, String>`. `Drop` kills the child and joins the monitor.

- [ ] **Step 1: Add imports and the FIFO path helper**

At the top of `src/audio/airplay.rs` (below the doc header, above the constants), add:

```rust
use std::ffi::CString;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
```

After the constants, add:

```rust
/// Base path for the per-receiver audio FIFOs shairport-sync writes.
const FIFO_PATH_BASE: &str = "/tmp/audioshare-airplay";
/// Path of the shairport-sync config file the supervisor writes per receiver.
const CONFIG_PATH_BASE: &str = "/tmp/audioshare-shairport";
/// How long to wait before relaunching a shairport-sync that exited.
const SHAIRPORT_RESTART_DELAY: Duration = Duration::from_secs(1);

/// Path of the audio FIFO backing receiver `index`.
pub fn fifo_path(index: usize) -> PathBuf {
    PathBuf::from(format!("{FIFO_PATH_BASE}-{index}.pcm"))
}
```

- [ ] **Step 2: Write the failing config-builder test**

Add to the `tests` module:

```rust
#[test]
fn config_sets_name_port_and_pipe() {
    let cfg = shairport_config("Audio Share (Hub)", 5000, Path::new("/tmp/x.pcm"));
    assert!(cfg.contains("name = \"Audio Share (Hub)\""), "{cfg}");
    assert!(cfg.contains("port = 5000"), "{cfg}");
    assert!(cfg.contains("name = \"/tmp/x.pcm\""), "{cfg}"); // pipe.name
    assert!(cfg.contains("pipe ="), "{cfg}");
}

#[test]
fn fifo_path_is_indexed() {
    assert_eq!(fifo_path(0), PathBuf::from("/tmp/audioshare-airplay-0.pcm"));
    assert_eq!(fifo_path(3), PathBuf::from("/tmp/audioshare-airplay-3.pcm"));
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib audio::airplay::tests::config_sets_name_port_and_pipe`
Expected: FAIL to compile — `cannot find function `shairport_config``.

- [ ] **Step 4: Implement the config builder, FIFO creation, spawn, monitor, and Drop**

Add (above the `#[cfg(test)]` block):

```rust
/// Build a minimal libconfig `shairport-sync` config: a named classic-AirPlay
/// receiver on `port` whose `pipe` backend writes raw PCM to `fifo_path`.
fn shairport_config(name: &str, port: u16, fifo_path: &Path) -> String {
    format!(
        "general =\n{{\n  name = \"{name}\";\n  port = {port};\n}};\n\n\
         pipe =\n{{\n  name = \"{}\";\n}};\n",
        fifo_path.display()
    )
}

/// Create the FIFO at `path` if it does not already exist (mode 0o600).
fn ensure_fifo(path: &Path) -> Result<(), String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| format!("bad fifo path {}: {e}", path.display()))?;
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != ErrorKind::AlreadyExists {
            return Err(format!("mkfifo {} failed: {err}", path.display()));
        }
    }
    Ok(())
}

/// Supervises one classic `shairport-sync` process writing PCM to a FIFO.
///
/// A thin supervisor (the same shape as `snapcast::SnapserverSupervisor`):
/// ensures the FIFO + config exist, launches `shairport-sync -c <conf> -o pipe`,
/// relaunches it if it exits, and kills it on drop. Per the plan, Snapcast and
/// AirPlay alike stay swappable implementation details behind the AudioSink seam.
pub struct ShairportSupervisor {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    monitor: Option<JoinHandle<()>>,
}

impl ShairportSupervisor {
    /// Spawn `shairport-sync` (resolved from `PATH`) as a receiver named `name`
    /// on `port`, writing PCM to `fifo`.
    pub fn spawn(name: &str, port: u16, fifo: &Path) -> Result<Self, String> {
        Self::spawn_with("shairport-sync", name, port, fifo)
    }

    /// Like [`spawn`](Self::spawn) but with an explicit binary (for tests/dev).
    pub fn spawn_with(
        binary: impl Into<String>,
        name: &str,
        port: u16,
        fifo: &Path,
    ) -> Result<Self, String> {
        let binary = binary.into();
        ensure_fifo(fifo)?;

        let config_path = PathBuf::from(format!("{CONFIG_PATH_BASE}-{port}.conf"));
        std::fs::write(&config_path, shairport_config(name, port, fifo))
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
}

impl Drop for ShairportSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().expect("shairport child mutex poisoned").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

/// Wait on the current child, relaunching after a short delay if it exits and we
/// are not stopping.
fn monitor_loop(binary: &str, config_path: &Path, stop: &AtomicBool, child: &Mutex<Option<Child>>) {
    loop {
        let current = child.lock().expect("shairport child mutex poisoned").take();
        if let Some(mut current) = current {
            let _ = current.wait();
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(SHAIRPORT_RESTART_DELAY);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match spawn_shairport(binary, config_path) {
            Ok(next) => *child.lock().expect("shairport child mutex poisoned") = Some(next),
            Err(e) => {
                eprintln!("shairport-sync relaunch failed: {e}");
                return;
            }
        }
    }
}

/// Spawn one classic `shairport-sync` configured by the file at `config_path`,
/// selecting the pipe output backend.
fn spawn_shairport(binary: &str, config_path: &Path) -> Result<Child, String> {
    Command::new(binary)
        .arg("-c")
        .arg(config_path)
        .arg("-o")
        .arg("pipe")
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn shairport-sync `{binary}`: {e}"))
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib audio::airplay`
Expected: PASS (conversion tests + the two new config/path tests). No build warnings.

- [ ] **Step 6: Commit**

```bash
git add src/audio/airplay.rs
git commit -m "AirPlay slice 1: ShairportSupervisor (spawn/restart/kill classic shairport-sync)"
```

---

### Task 4: `pump_fifo_to_sink` — read the FIFO into an `AudioSink`

The reader: open the FIFO (blocking until shairport writes on session start), read `s16le`, convert + mix + resample to the sink, loop. On EOF (session end) reopen and wait for the next session. Stop is checked between reads.

**Files:**
- Modify: `src/audio/airplay.rs`
- Test: inline `#[cfg(test)]` in `src/audio/airplay.rs`

**Interfaces:**
- Consumes: `i16le_to_planar_f32` (Task 1); `AIRPLAY_SAMPLE_RATE`/`AIRPLAY_CHANNELS` (Task 1); `crate::audio::decode::{ResamplePipeline, mix_planar}` (Task 2); `crate::audio::sink::AudioSink`.
- Produces: `pub fn pump_fifo_to_sink(fifo: &std::path::Path, sink: &dyn AudioSink, stop: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<(), String>`.

- [ ] **Step 1: Add the reader's imports**

At the top of `src/audio/airplay.rs`, extend the `std::io` import and add the sink/decode uses:

```rust
use std::fs::File;
use std::io::Read;

use crate::audio::decode::{mix_planar, ResamplePipeline};
use crate::audio::sink::AudioSink;
```

(Keep the existing `use std::io::ErrorKind;` — or merge into `use std::io::{ErrorKind, Read};`.)

- [ ] **Step 2: Write the failing test (temp FIFO + writer + capturing sink)**

Add to the `tests` module. The sink reports 44100/2 so there's no resampling — captured output must equal the input samples (within float tolerance), proving the read→convert→write path:

```rust
#[test]
fn pump_passes_through_at_native_rate() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    // A sink at AirPlay's native rate/channels: no resampling, exact passthrough.
    struct Capture(std::sync::Mutex<Vec<f32>>);
    impl AudioSink for Capture {
        fn sample_rate(&self) -> u32 { AIRPLAY_SAMPLE_RATE }
        fn channels(&self) -> u16 { AIRPLAY_CHANNELS as u16 }
        fn write(&self, samples: &[f32]) {
            self.0.lock().unwrap().extend_from_slice(samples);
        }
    }

    let path = std::env::temp_dir().join(format!("as-airplay-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo failed");

    // Known interleaved stereo samples -> s16le bytes.
    let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 0.25, 1.0, -1.0];
    let mut bytes = Vec::new();
    for &s in &samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }

    // Writer thread: open the FIFO for writing (blocks until the reader opens
    // the read end), write all bytes, then close to signal EOF.
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&writer_path).unwrap();
        f.write_all(&bytes).unwrap();
        // Dropping `f` closes the write end -> reader sees EOF.
    });

    let sink = Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
    let stop = Arc::new(AtomicBool::new(false));

    // Reader thread: pump until the first session ends, then stop.
    let reader_sink = Arc::clone(&sink);
    let reader_stop = Arc::clone(&stop);
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        // Stop after the first EOF so the test terminates: a tiny wrapper that
        // pumps one session. We set stop right after the writer finishes.
        let _ = pump_one_session(&reader_path, reader_sink.as_ref(), &reader_stop);
    });

    writer.join().unwrap();
    reader.join().unwrap();

    let got = sink.0.lock().unwrap().clone();
    let _ = std::fs::remove_file(&path);
    assert_eq!(got.len(), samples.len(), "all frames delivered");
    for (g, s) in got.iter().zip(samples.iter()) {
        assert!((g - s).abs() < 1e-3, "passthrough sample mismatch: {g} vs {s}");
    }
}
```

> Note: the test calls `pump_one_session` — a small helper that runs exactly one open→read-to-EOF→return cycle, so the test is deterministic. `pump_fifo_to_sink` is the outer loop that repeats it until `stop`.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib audio::airplay::tests::pump_passes_through_at_native_rate`
Expected: FAIL to compile — `cannot find function `pump_one_session`` / `pump_fifo_to_sink``.

- [ ] **Step 4: Implement the reader**

Add (above the `#[cfg(test)]` block):

```rust
/// Read one AirPlay session from `fifo` into `sink`: open the FIFO (blocking
/// until shairport-sync opens the write end on session start), then read until
/// EOF (session end) or `stop`. Returns `Ok(())` on a clean EOF.
fn pump_one_session(
    fifo: &Path,
    sink: &dyn AudioSink,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    // Blocking open: returns once a writer (shairport) is present.
    let mut file = File::open(fifo)
        .map_err(|e| format!("open airplay fifo {} failed: {e}", fifo.display()))?;

    let mut pipeline = ResamplePipeline::new(
        AIRPLAY_SAMPLE_RATE,
        AIRPLAY_CHANNELS,
        sink.sample_rate(),
        sink.channels() as usize,
    )?;

    let frame_bytes = AIRPLAY_CHANNELS * 2;
    let mut remainder: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];

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
        let planar = i16le_to_planar_f32(&remainder[..whole], AIRPLAY_CHANNELS);
        remainder.drain(..whole);

        let mixed = mix_planar(&planar, sink.channels() as usize);
        pipeline.push_and_drain(mixed, sink);
    }
}

/// Continuously receive AirPlay audio from `fifo` into `sink` until `stop` is
/// set: each session is one [`pump_one_session`] cycle; between sessions the
/// blocking FIFO open parks the thread until the next sender connects.
pub fn pump_fifo_to_sink(
    fifo: &Path,
    sink: &dyn AudioSink,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        pump_one_session(fifo, sink, stop)?;
    }
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib audio::airplay`
Expected: PASS (all conversion, config, and the new pump test).

- [ ] **Step 6: Commit**

```bash
git add src/audio/airplay.rs
git commit -m "AirPlay slice 1: pump_fifo_to_sink reader (s16le -> resample -> AudioSink)"
```

---

### Task 5: Demo-gated end-to-end test + setup notes

Wire the supervisor + reader to a real `AudioOutput` behind `#[ignore]`, mirroring `plays_to_snapcast_briefly`. This is the manual proof: AirPlay from a phone to the hub and hear it.

**Files:**
- Modify: `src/audio/airplay.rs` (add the ignored test)
- Modify: `docs/multi-room-plan.md` (append an AirPlay bring-up note)

**Interfaces:**
- Consumes: `ShairportSupervisor::spawn` (Task 3), `pump_fifo_to_sink` (Task 4), `fifo_path` (Task 3), `crate::audio::output::AudioOutput`.

- [ ] **Step 1: Add the ignored end-to-end test**

Add to the `tests` module:

```rust
// Live end-to-end check (opt-in: needs the classic `shairport-sync` binary and
// audio hardware). Proves Slice 1's receive path with zero engine wiring:
//   cargo test audio::airplay::tests::receives_airplay_briefly -- --ignored --nocapture
// While it runs, on your iPhone/Mac open the AirPlay menu, pick the receiver
// named "Audio Share (Hub)", and play something. You should hear it from this
// machine's default output for ~30s.
#[test]
#[ignore]
fn receives_airplay_briefly() {
    use crate::audio::output::AudioOutput;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let fifo = fifo_path(0);
    let _server = ShairportSupervisor::spawn("Audio Share (Hub)", 5000, &fifo)
        .expect("shairport-sync should spawn (is the classic build installed?)");

    let output = Arc::new(AudioOutput::new().expect("open default output device"));
    let stop = Arc::new(AtomicBool::new(false));

    let pump_output = Arc::clone(&output);
    let pump_stop = Arc::clone(&stop);
    let pump_fifo = fifo.clone();
    let worker = thread::spawn(move || {
        pump_fifo_to_sink(&pump_fifo, pump_output.as_ref(), &pump_stop)
    });

    thread::sleep(Duration::from_secs(30));
    stop.store(true, Ordering::Relaxed);
    // The pump may be parked in a blocking FIFO read/open; this test does not
    // join it (the process exits at test end). Stop responsiveness during an
    // active read is a Slice 2 concern (session tracking via the metadata pipe).
    drop(worker);
}
```

- [ ] **Step 2: Verify it compiles and is skipped by default**

Run: `cargo test --lib audio::airplay`
Expected: PASS — the ignored test compiles and is reported as `ignored`, the rest pass.

- [ ] **Step 3: (Manual, optional) run the demo on the Pi/Linux box**

Prereqs (document, don't script): install the **classic** `shairport-sync` (`apt install shairport-sync` is classic by default — verify it is *not* an AirPlay-2 build), and **disable the distro service so it doesn't squat the AirPlay name/ports**:

```bash
sudo systemctl disable --now shairport-sync
```

Then: `cargo test audio::airplay::tests::receives_airplay_briefly -- --ignored --nocapture`, pick "Audio Share (Hub)" in the iPhone AirPlay menu, and confirm you hear audio.

- [ ] **Step 4: Append the bring-up note to the multi-room plan**

Add a short subsection at the end of `docs/multi-room-plan.md` recording: classic `shairport-sync` is required for multi-instance (Slice 2+); `sudo systemctl disable --now shairport-sync` is the AirPlay analog of the `snapserver.service` collision; Slice 1 proves the path standalone (no engine wiring), engine/`ShairportManager` is Slice 2.

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay.rs docs/multi-room-plan.md
git commit -m "AirPlay slice 1: demo-gated end-to-end test + bring-up notes"
```

---

## Self-Review

**Spec coverage (Slice 1 scope only):**
- "ShairportSupervisor + config/arg building" → Task 3. ✅
- "the PCM reader" → Task 4 (`pump_fifo_to_sink`), with conversion in Task 1 and resampling reused via Task 2. ✅
- "prove the path to the hub's local output" / demo → Task 5 (ignored end-to-end against `AudioOutput`). ✅
- **Deviation noted:** the spec's Slice 1 line says "one source wired into the engine." This plan instead proves the path **standalone** (no `Engine`/server wiring) and defers wiring to Slice 2's `ShairportManager` — deliberately mirroring how `snapcast` sub-step 1 proved its path before sub-step 2 wired it in (lower risk, demoable now). Slice 2 of the spec already owns the manager + engine wiring, so nothing is dropped. Flag this to the user.
- Metadata, per-zone receivers, `sources` push, `reroute`, `get_art` are **Slices 2–4**, intentionally out of this plan.

**Placeholder scan:** No TBD/TODO/"handle edge cases"/"similar to". Every code step shows complete code; commands have expected output. ✅

**Type consistency:** `i16le_to_planar_f32(&[u8], usize) -> Vec<Vec<f32>>` (Task 1) consumed by `pump_one_session` (Task 4) with `AIRPLAY_CHANNELS`. `ResamplePipeline::new(in_rate, in_channels, out_rate, out_channels)` + `push_and_drain(mixed, &dyn AudioSink)` and `mix_planar(&[Vec<f32>], usize)` (Task 2) match decode.rs's real signatures and Task 4's calls. `fifo_path(usize) -> PathBuf` (Task 3) used by Task 5. `ShairportSupervisor::spawn(&str, u16, &Path)` (Task 3) used by Task 5. `pump_fifo_to_sink(&Path, &dyn AudioSink, &Arc<AtomicBool>)` (Task 4) used by Task 5. ✅
