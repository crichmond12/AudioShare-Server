# Multi-Room Sub-step 3 — Hub-Driven Snapcast Streams & Grouping — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make zone membership real on the hub — independent per-dongle audio plus synchronized multi-dongle groups — by programming `snapserver` over its JSON-RPC API from a hub-owned desired-state reconciler, and add zone CRUD to the wire protocol.

**Architecture:** The engine keeps the zone model as the single source of truth. A new `SnapcastRouter` owns a fixed pool of `snapserver` pipe streams, a JSON-RPC command connection, and a desired-state reconciler that converges `snapserver`'s groups/streams to the hub's intent — triggered both by zone/playback changes and by `snapserver` client-connect notifications. Snapcast stays behind the `AudioSink` seam; the engine never speaks JSON-RPC.

**Tech Stack:** Rust 2021 (workspace root crate `audio_share`), tokio (server only — the audio/snapcast path is std threads), `serde_json`, `libc` (FIFO `fcntl`/`mkfifo`), `snapserver`/`snapclient` external binaries.

## Global Constraints

- The spec is `docs/superpowers/specs/2026-06-21-multiroom-substep3-snapcast-grouping-design.md`. Read it before starting.
- **Device-free tests must stay green without hardware/binaries:** `cargo test` (workspace) runs in CI with no audio device, no `snapserver`, no `snapclient`. Anything needing those is `#[ignore]` (demo-gated).
- **Constructing `Engine` / `SnapcastRouter` opens nothing** — no audio device, no `snapserver` process, no TCP. Processes/sockets start lazily on the first dongle registration (`ensure_snapcast` → `ensure_started`), mirroring `Engine::ensure_local`.
- **The engine never speaks JSON-RPC directly** — all Snapcast control goes through `SnapcastRouter`.
- **A synced zone is dongles-only.** The hub local output (`"local"`, zone `"default"`) can never share a zone with a dongle. Enforce in `set_zone_outputs`.
- **Stream pool size `STREAM_POOL_SIZE = 16`** — the cap on concurrently *playing* zones. Creating zones is unbounded.
- FIFO paths: `/tmp/audioshare-snapfifo-{k}` for `k in 0..16`. Stream ids: `as-{k}`.
- Snapcast control port is **1705** on `127.0.0.1` (snapserver runs locally on the hub). Snapcast audio port stays **1704** (`DEFAULT_SNAPSERVER_PORT` in `audioshare_protocol`).
- Match existing house style: `expect("… mutex poisoned")` on lock, module-level `//!` doc comments, kill-on-drop process supervisors, TDD with one behavior per test.
- Run all tests from the repo root with `cargo test` (workspace). Run a single test with `cargo test <path>::<name> -- --nocapture`.

---

## File Structure

- Modify `src/audio/snapcast.rs` — `SnapcastSink` backpressure (clear `O_NONBLOCK` after open); `SnapserverSupervisor` launches N pipe streams; add `fifo_path(k)` / `stream_id(k)` helpers; `DEFAULT_FIFO_PATH` stays for the ignored single-stream test (now `fifo_path(0)`).
- Create `src/audio/snapcast_control.rs` — `ServerStatus`/`GroupInfo` types + parsing, `SnapcastControl` trait, `CommandConn` (synchronous JSON-RPC over TCP), `EventListener` (notification reader thread).
- Create `src/audio/snapcast_router.rs` — `STREAM_POOL_SIZE`, `StreamPool`, `ZoneRouting`, pure `reconcile()`, and `SnapcastRouter` (owns supervisor + pool + command conn + event listener + desired state).
- Modify `src/audio/registry.rs` — `Output.sink` becomes `Option<Arc<dyn AudioSink>>` (dongles have `None`).
- Modify `src/audio/engine.rs` — replace the shared `snapcast_sink` with a `SnapcastRouter`; route dongle zones through it; add zone CRUD (`create_zone`/`delete_zone`/`rename_zone`/`set_zone_outputs`) + `list_zones`; `snapcast_on_notify` reconcile hook.
- Modify `src/audio/mod.rs` — declare `snapcast_control` and `snapcast_router`.
- Modify `src/server/commands.rs` — new tasks `create_zone`/`delete_zone`/`rename_zone`/`set_zone_outputs`; map `no_free_stream`/`mixed_zone_unsupported`/`unknown_output`.
- Modify `src/server/connection.rs` — push a `zones` message alongside `outputs`; handle a `list_zones` pull.
- Modify `CLAUDE.md` and `docs/multi-room-plan.md` — protocol/state + plan status.

---

## Sub-step 3.1 — `SnapcastSink` backpressure

### Task 1: `SnapcastSink` blocks once `snapserver` is reading

**Files:**
- Modify: `src/audio/snapcast.rs` (the `SnapcastSink::write` open path + a new `clear_nonblocking` helper)
- Test: `src/audio/snapcast.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `open_fifo_write(&Path) -> io::Result<File>`, `to_i16le`.
- Produces: `fn clear_nonblocking(file: &File) -> std::io::Result<()>`; unchanged `SnapcastSink` public API.

- [ ] **Step 1: Write the failing test**

Add to `src/audio/snapcast.rs` tests. This proves backpressure: a slow reader drains a real FIFO while the sink writes far more than the pipe buffer (64 KiB); with blocking writes the reader receives *every* byte (no drops). Device-free — no `snapserver`, no audio.

```rust
    #[test]
    fn write_applies_backpressure_once_reader_present() {
        use std::ffi::CString;
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        // Unique FIFO path in the temp dir.
        let path = std::env::temp_dir().join(format!("as-bp-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let c_path = CString::new(path.to_str().unwrap()).unwrap();
        // mkfifo(path, 0o600)
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo failed");

        // Total bytes well past the 64 KiB pipe capacity, so a non-blocking
        // dropping writer could not deliver all of them.
        const TOTAL_SAMPLES: usize = 200_000; // -> 400_000 bytes of s16le
        const TOTAL_BYTES: usize = TOTAL_SAMPLES * 2;

        // Reader: open the FIFO and drain it slowly until it has all the bytes.
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .open(&reader_path)
                .expect("open fifo for read");
            let mut got = 0usize;
            let mut buf = [0u8; 4096];
            while got < TOTAL_BYTES {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        got += n;
                        std::thread::sleep(std::time::Duration::from_micros(50));
                    }
                    Err(_) => break,
                }
            }
            got
        });

        let sink = SnapcastSink::new(&path);
        // Write in chunks; with backpressure these block instead of dropping.
        let chunk = vec![0.25f32; 4096];
        let mut written = 0usize;
        while written < TOTAL_SAMPLES {
            sink.write(&chunk);
            written += chunk.len();
        }

        let got = reader.join().expect("reader thread");
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, TOTAL_BYTES, "reader must receive every byte (no drops)");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast::tests::write_applies_backpressure_once_reader_present -- --nocapture`
Expected: FAIL — with the current non-blocking-drop write the reader receives fewer than `TOTAL_BYTES` (assert_eq fails), or `clear_nonblocking` is undefined if you wrote the impl first.

- [ ] **Step 3: Write minimal implementation**

In `src/audio/snapcast.rs`, add the helper near `open_fifo_write`:

```rust
/// Clear `O_NONBLOCK` on an open FIFO writer so subsequent writes block.
///
/// We open the FIFO non-blocking (so the open returns `ENXIO` instead of
/// stalling when `snapserver` isn't reading yet). Once the open *succeeds* a
/// reader is present, and a **blocking** write is the natural backpressure that
/// paces the decode thread — far better than dropping on a full pipe, which
/// causes the KAN-23 Snapcast-variant choppiness. If the reader later vanishes a
/// blocking write returns `EPIPE` (Rust ignores `SIGPIPE`), which the caller
/// already handles by dropping the handle.
fn clear_nonblocking(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let res = unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    if res < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
```

Then change the lazy-open branch in `SnapcastSink::write` to clear non-blocking on success:

```rust
        if inner.writer.is_none() {
            match open_fifo_write(&self.fifo_path) {
                Ok(file) => {
                    // Reader is present (open succeeded): switch to blocking so
                    // writes pace the decode thread instead of dropping overflow.
                    if clear_nonblocking(&file).is_err() {
                        return;
                    }
                    inner.writer = Some(file);
                }
                Err(_) => return,
            }
        }
```

The existing `WouldBlock` arm in the write match can stay (harmless now that writes block), but update its comment to note blocking writes shouldn't normally hit it.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast::tests::write_applies_backpressure_once_reader_present -- --nocapture`
Expected: PASS. Also run `cargo test audio::snapcast` — the existing `write_without_reader_is_a_silent_drop` must still pass (no reader → `ENXIO` → silent drop, unchanged).

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast.rs
git commit -m "Sub-step 3.1: SnapcastSink applies backpressure once snapserver reads"
```

---

## Sub-step 3.2 — Multi-stream supervisor + stream pool

### Task 2: `SnapserverSupervisor` launches N pipe streams

**Files:**
- Modify: `src/audio/snapcast.rs` (`fifo_path`/`stream_id` helpers; `SnapserverSupervisor::spawn`/`spawn_with`; `pipe_source`; update the ignored e2e test)
- Test: `src/audio/snapcast.rs` tests

**Interfaces:**
- Produces: `pub fn fifo_path(index: usize) -> std::path::PathBuf`; `pub fn stream_id(index: usize) -> String`; `SnapserverSupervisor::spawn(stream_count: usize) -> Result<Self, String>`; `SnapserverSupervisor::spawn_with(binary, stream_count) -> Result<Self, String>`.
- Consumes: existing `SNAPCAST_SAMPLE_FORMAT`, `spawn_snapserver`, `monitor_loop`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn fifo_and_stream_ids_are_indexed() {
        assert_eq!(fifo_path(0), std::path::PathBuf::from("/tmp/audioshare-snapfifo-0"));
        assert_eq!(fifo_path(7), std::path::PathBuf::from("/tmp/audioshare-snapfifo-7"));
        assert_eq!(stream_id(0), "as-0");
        assert_eq!(stream_id(7), "as-7");
    }

    #[test]
    fn pipe_source_names_the_indexed_stream() {
        let source = pipe_source(3);
        assert!(source.contains("pipe:///tmp/audioshare-snapfifo-3"), "{source}");
        assert!(source.contains("name=as-3"), "{source}");
        assert!(source.contains("mode=create"));
        assert!(source.contains("sampleformat=48000:16:2"));
        assert!(source.contains("codec=pcm"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast::tests::fifo_and_stream_ids_are_indexed -- --nocapture`
Expected: FAIL — `fifo_path`/`stream_id` undefined; `pipe_source` signature mismatch.

- [ ] **Step 3: Write minimal implementation**

Replace the FIFO constant + `pipe_source` and the supervisor spawn surface in `src/audio/snapcast.rs`:

```rust
/// Base path for the per-stream FIFOs snapserver reads. Each pool stream `k`
/// reads `audioshare-snapfifo-{k}`.
const FIFO_PATH_BASE: &str = "/tmp/audioshare-snapfifo";

/// Path of the FIFO backing pool stream `index`.
pub fn fifo_path(index: usize) -> PathBuf {
    PathBuf::from(format!("{FIFO_PATH_BASE}-{index}"))
}

/// snapserver stream name for pool stream `index` (the `stream_id` the control
/// API addresses, e.g. for `Group.SetStream`).
pub fn stream_id(index: usize) -> String {
    format!("as-{index}")
}

/// Kept for the ignored single-stream smoke test: stream 0's FIFO.
pub const DEFAULT_FIFO_PATH: &str = "/tmp/audioshare-snapfifo-0";

/// Build the `snapserver` pipe-stream source URI for pool stream `index`.
fn pipe_source(index: usize) -> String {
    format!(
        "pipe://{}?name={}&mode=create&sampleformat={}&codec=pcm",
        fifo_path(index).display(),
        stream_id(index),
        SNAPCAST_SAMPLE_FORMAT
    )
}
```

Change `SnapserverSupervisor::spawn`/`spawn_with` to launch `stream_count` streams (one `--stream.source` per stream). The monitor relaunches with the same full source list:

```rust
    /// Spawn `snapserver` (resolved from `PATH`) with `stream_count` pipe streams
    /// `as-0..as-(stream_count-1)`, each reading its own FIFO.
    pub fn spawn(stream_count: usize) -> Result<Self, String> {
        Self::spawn_with("snapserver", stream_count)
    }

    /// Like [`spawn`](Self::spawn) but with an explicit binary (for tests/dev).
    pub fn spawn_with(binary: impl Into<String>, stream_count: usize) -> Result<Self, String> {
        let binary = binary.into();
        let sources: Vec<String> = (0..stream_count).map(pipe_source).collect();

        let first = spawn_snapserver(&binary, &sources)?;

        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(Some(first)));

        let monitor = {
            let stop = Arc::clone(&stop);
            let child = Arc::clone(&child);
            let binary = binary.clone();
            let sources = sources.clone();
            thread::Builder::new()
                .name("snapserver-supervisor".to_string())
                .spawn(move || monitor_loop(&binary, &sources, &stop, &child))
                .map_err(|e| format!("failed to spawn snapserver supervisor thread: {e}"))?
        };

        Ok(Self { stop, child, monitor: Some(monitor) })
    }
```

Update `monitor_loop` and `spawn_snapserver` to take `sources: &[String]` and emit one `--stream.source <s>` per source:

```rust
fn monitor_loop(binary: &str, sources: &[String], stop: &AtomicBool, child: &Mutex<Option<Child>>) {
    loop {
        let current = child.lock().expect("snapserver child mutex poisoned").take();
        if let Some(mut current) = current {
            let _ = current.wait();
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(SNAPSERVER_RESTART_DELAY);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match spawn_snapserver(binary, sources) {
            Ok(next) => *child.lock().expect("snapserver child mutex poisoned") = Some(next),
            Err(e) => {
                eprintln!("snapserver relaunch failed: {e}");
                return;
            }
        }
    }
}

fn spawn_snapserver(binary: &str, sources: &[String]) -> Result<Child, String> {
    let mut cmd = Command::new(binary);
    for source in sources {
        cmd.arg("--stream.source").arg(source);
    }
    cmd.stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn snapserver `{binary}`: {e}"))
}
```

Update the ignored `plays_to_snapcast_briefly` test: call `SnapserverSupervisor::spawn(1)` and `SnapcastSink::new(fifo_path(0))`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast -- --nocapture`
Expected: PASS (the new helper tests; existing device-free tests still green). The ignored e2e stays ignored.

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast.rs
git commit -m "Sub-step 3.2: snapserver supervisor launches N indexed pipe streams"
```

### Task 3: `StreamPool` allocation

**Files:**
- Create: `src/audio/snapcast_router.rs`
- Modify: `src/audio/mod.rs` (add `pub mod snapcast_router;`)
- Test: `src/audio/snapcast_router.rs` tests

**Interfaces:**
- Consumes: `crate::audio::snapcast::{SnapcastSink, fifo_path, stream_id}`; `crate::audio::sink::AudioSink`.
- Produces: `pub const STREAM_POOL_SIZE: usize`; `pub struct AllocatedStream { pub stream_id: String, pub sink: Arc<dyn AudioSink> }`; `StreamPool::new(size)`, `StreamPool::allocate(&mut self, zone: &str) -> Option<AllocatedStream>`, `StreamPool::release(&mut self, zone: &str)`, `StreamPool::stream_for(&self, zone: &str) -> Option<String>`.

- [ ] **Step 1: Write the failing test**

Create `src/audio/snapcast_router.rs` with a module doc comment and these tests:

```rust
    #[test]
    fn allocate_reuses_the_same_slot_for_a_zone() {
        let mut pool = StreamPool::new(2);
        let a = pool.allocate("kitchen").expect("first alloc");
        let b = pool.allocate("kitchen").expect("re-alloc same zone");
        assert_eq!(a.stream_id, b.stream_id, "same zone keeps its slot");
        assert_eq!(pool.stream_for("kitchen").as_deref(), Some(a.stream_id.as_str()));
    }

    #[test]
    fn allocate_gives_distinct_streams_to_distinct_zones() {
        let mut pool = StreamPool::new(2);
        let a = pool.allocate("kitchen").unwrap();
        let b = pool.allocate("bedroom").unwrap();
        assert_ne!(a.stream_id, b.stream_id);
    }

    #[test]
    fn allocate_returns_none_when_exhausted() {
        let mut pool = StreamPool::new(1);
        assert!(pool.allocate("kitchen").is_some());
        assert!(pool.allocate("bedroom").is_none(), "pool of 1 has no slot left");
    }

    #[test]
    fn release_frees_the_slot_for_reuse() {
        let mut pool = StreamPool::new(1);
        let a = pool.allocate("kitchen").unwrap();
        pool.release("kitchen");
        assert!(pool.stream_for("kitchen").is_none());
        let b = pool.allocate("bedroom").expect("slot freed");
        assert_eq!(a.stream_id, b.stream_id, "the freed slot is reused");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast_router::tests -- --nocapture`
Expected: FAIL — module/types undefined (won't compile).

- [ ] **Step 3: Write minimal implementation**

In `src/audio/snapcast_router.rs`:

```rust
//! Hub-driven Snapcast stream pool, group reconciler, and router
//! (multi-room Change 5, sub-step 3).
//!
//! The engine's single seam into Snapcast. It owns a fixed pool of `snapserver`
//! pipe streams (`StreamPool`), allocates one to each *playing* dongle zone, and
//! reconciles `snapserver`'s groups/streams to the hub's desired topology over
//! the control API. See `docs/multi-room-plan.md` Change 5 sub-step 3.

use std::collections::HashMap;
use std::sync::Arc;

use crate::audio::sink::AudioSink;
use crate::audio::snapcast::{fifo_path, stream_id, SnapcastSink};

/// Concurrent *playing* zones the hub supports (the snapserver stream pool size).
/// Creating zones is unbounded; only playback consumes a slot.
pub const STREAM_POOL_SIZE: usize = 16;

/// A stream handed to a zone: the snapserver `stream_id` to bind its group to,
/// and the sink the decode thread writes into.
pub struct AllocatedStream {
    pub stream_id: String,
    pub sink: Arc<dyn AudioSink>,
}

struct Slot {
    stream_id: String,
    sink: Arc<SnapcastSink>,
    allocated_to: Option<String>,
}

/// A fixed pool of snapserver pipe streams, allocated one-per-playing-zone.
pub struct StreamPool {
    slots: Vec<Slot>,
}

impl StreamPool {
    /// Build a pool of `size` slots, each backed by its own indexed FIFO sink.
    /// Constructs no I/O — the `SnapcastSink`s open their FIFOs lazily on write.
    pub fn new(size: usize) -> Self {
        let slots = (0..size)
            .map(|k| Slot {
                stream_id: stream_id(k),
                sink: Arc::new(SnapcastSink::new(fifo_path(k))),
                allocated_to: None,
            })
            .collect();
        Self { slots }
    }

    /// Reserve a stream for `zone`. Idempotent: a zone already holding a slot
    /// gets the same one back. Returns `None` only when every slot is taken.
    pub fn allocate(&mut self, zone: &str) -> Option<AllocatedStream> {
        if let Some(slot) = self.slots.iter().find(|s| s.allocated_to.as_deref() == Some(zone)) {
            return Some(AllocatedStream {
                stream_id: slot.stream_id.clone(),
                sink: Arc::clone(&slot.sink) as Arc<dyn AudioSink>,
            });
        }
        let slot = self.slots.iter_mut().find(|s| s.allocated_to.is_none())?;
        slot.allocated_to = Some(zone.to_string());
        Some(AllocatedStream {
            stream_id: slot.stream_id.clone(),
            sink: Arc::clone(&slot.sink) as Arc<dyn AudioSink>,
        })
    }

    /// Free `zone`'s slot, if any, for reuse. No-op if the zone holds none.
    pub fn release(&mut self, zone: &str) {
        for slot in &mut self.slots {
            if slot.allocated_to.as_deref() == Some(zone) {
                slot.allocated_to = None;
            }
        }
    }

    /// The `stream_id` currently allocated to `zone`, if any.
    pub fn stream_for(&self, zone: &str) -> Option<String> {
        self.slots
            .iter()
            .find(|s| s.allocated_to.as_deref() == Some(zone))
            .map(|s| s.stream_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // (tests from Step 1)
}
```

Add to `src/audio/mod.rs`: `pub mod snapcast_router;` (next to `pub mod snapcast;`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast_router::tests -- --nocapture`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast_router.rs src/audio/mod.rs
git commit -m "Sub-step 3.2: StreamPool allocates snapserver streams per zone"
```

---

## Sub-step 3.3 — JSON-RPC control client

### Task 4: `ServerStatus` parsing + `SnapcastControl` trait

**Files:**
- Create: `src/audio/snapcast_control.rs`
- Modify: `src/audio/mod.rs` (add `pub mod snapcast_control;`)
- Test: `src/audio/snapcast_control.rs` tests

**Interfaces:**
- Produces:
  - `pub struct GroupInfo { pub id: String, pub stream_id: String, pub clients: Vec<String> }`
  - `pub struct ServerStatus { pub groups: Vec<GroupInfo> }` with `pub fn group_of(&self, client_id: &str) -> Option<&str>`, `pub fn is_connected(&self, client_id: &str) -> bool`.
  - `pub fn parse_server_status(result: &serde_json::Value) -> Result<ServerStatus, String>` — parses a `Server.GetStatus` JSON-RPC `result`.
  - `pub trait SnapcastControl: Send + Sync { fn get_status(&self) -> Result<ServerStatus, String>; fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String>; fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String>; }`

- [ ] **Step 1: Write the failing test**

A realistic (trimmed) `Server.GetStatus` result shape: `result.server.groups[].{id,stream_id,clients[].{id,connected}}`.

```rust
    fn sample_status() -> serde_json::Value {
        serde_json::json!({
            "server": {
                "groups": [
                    {
                        "id": "group-A",
                        "stream_id": "as-0",
                        "clients": [
                            { "id": "dongle-1", "connected": true },
                            { "id": "dongle-2", "connected": false }
                        ]
                    },
                    {
                        "id": "group-B",
                        "stream_id": "as-1",
                        "clients": [ { "id": "dongle-3", "connected": true } ]
                    }
                ]
            }
        })
    }

    #[test]
    fn parses_groups_clients_and_streams() {
        let status = parse_server_status(&sample_status()).expect("parse");
        assert_eq!(status.groups.len(), 2);
        assert_eq!(status.group_of("dongle-1"), Some("group-A"));
        assert_eq!(status.group_of("dongle-3"), Some("group-B"));
        assert_eq!(status.group_of("nope"), None);
        assert!(status.is_connected("dongle-1"));
        assert!(!status.is_connected("dongle-2"));
        assert!(!status.is_connected("nope"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast_control::tests::parses_groups_clients_and_streams -- --nocapture`
Expected: FAIL — module/types undefined.

- [ ] **Step 3: Write minimal implementation**

Create `src/audio/snapcast_control.rs`:

```rust
//! Snapcast control (JSON-RPC) client (multi-room Change 5, sub-step 3).
//!
//! A thin, synchronous client for `snapserver`'s control API on port 1705
//! (newline-delimited JSON-RPC 2.0). Only the oldest, most stable methods are
//! used — `Server.GetStatus`, `Group.SetClients`, `Group.SetStream` — so the hub
//! is insulated from snapserver version drift. Snapcast stays an implementation
//! detail behind [`SnapcastControl`]; the engine never sees this type.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

/// One snapserver group: its id, the stream it plays, and its client ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupInfo {
    pub id: String,
    pub stream_id: String,
    pub clients: Vec<String>,
}

/// The slice of `Server.GetStatus` the reconciler needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerStatus {
    pub groups: Vec<GroupInfo>,
}

impl ServerStatus {
    /// The id of the group currently containing `client_id`, if any.
    pub fn group_of(&self, client_id: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|g| g.clients.iter().any(|c| c == client_id))
            .map(|g| g.id.as_str())
    }

    /// Whether a connected client with this id exists in any group.
    pub fn is_connected(&self, client_id: &str) -> bool {
        self.connected.iter().any(|c| c == client_id)
    }
}
```

Parsing needs to also remember which clients are *connected* (the `clients` vec on a `GroupInfo` includes disconnected ones so `group_of` can still find a recently-dropped client). Add a private `connected: Vec<String>` field to `ServerStatus` (and `#[derive(Default)]` covers it). Implement parsing:

```rust
/// Parse a `Server.GetStatus` JSON-RPC `result` into a [`ServerStatus`].
pub fn parse_server_status(result: &Value) -> Result<ServerStatus, String> {
    let groups_json = result["server"]["groups"]
        .as_array()
        .ok_or_else(|| "GetStatus: missing server.groups".to_string())?;

    let mut groups = Vec::with_capacity(groups_json.len());
    let mut connected = Vec::new();
    for g in groups_json {
        let id = g["id"].as_str().unwrap_or_default().to_string();
        let stream_id = g["stream_id"].as_str().unwrap_or_default().to_string();
        let mut clients = Vec::new();
        if let Some(cs) = g["clients"].as_array() {
            for c in cs {
                let cid = c["id"].as_str().unwrap_or_default().to_string();
                if c["connected"].as_bool().unwrap_or(false) {
                    connected.push(cid.clone());
                }
                clients.push(cid);
            }
        }
        groups.push(GroupInfo { id, stream_id, clients });
    }
    Ok(ServerStatus { groups, connected })
}
```

Update the struct to carry `connected`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerStatus {
    pub groups: Vec<GroupInfo>,
    connected: Vec<String>,
}
```

Add the trait:

```rust
/// Hub → snapserver control surface, behind a trait so the reconciler can be
/// unit-tested against a mock with no real snapserver.
pub trait SnapcastControl: Send + Sync {
    fn get_status(&self) -> Result<ServerStatus, String>;
    fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String>;
    fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String>;
}
```

Add `pub mod snapcast_control;` to `src/audio/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast_control::tests::parses_groups_clients_and_streams -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast_control.rs src/audio/mod.rs
git commit -m "Sub-step 3.3: snapserver GetStatus parsing + SnapcastControl trait"
```

### Task 5: `CommandConn` — synchronous JSON-RPC over TCP

**Files:**
- Modify: `src/audio/snapcast_control.rs` (add `CommandConn` + `EventListener`)
- Test: `src/audio/snapcast_control.rs` tests (loopback mock snapserver)

**Interfaces:**
- Consumes: `parse_server_status`, `SnapcastControl`.
- Produces:
  - `pub struct CommandConn` with `pub fn connect(host: &str, port: u16) -> Result<Self, String>` and `impl SnapcastControl for CommandConn`.
  - `pub struct EventListener` with `pub fn spawn(host: &str, port: u16, on_event: impl Fn() + Send + 'static) -> Result<Self, String>` and kill-on-drop.

- [ ] **Step 1: Write the failing test**

A loopback "mock snapserver" that accepts one connection, reads one request line, asserts it's `Server.GetStatus`, and replies with a JSON-RPC result. Proves request framing + response parsing without a real snapserver.

```rust
    #[test]
    fn command_conn_get_status_round_trips_over_tcp() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(req["method"], "Server.GetStatus");
            let id = req["id"].clone();
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "server": { "groups": [
                        { "id": "g0", "stream_id": "as-0",
                          "clients": [ { "id": "d1", "connected": true } ] }
                    ] }
                }
            });
            let mut stream = stream;
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            stream.write_all(&bytes).unwrap();
        });

        let conn = CommandConn::connect("127.0.0.1", addr.port()).expect("connect");
        let status = conn.get_status().expect("get_status");
        assert_eq!(status.group_of("d1"), Some("g0"));
        server.join().unwrap();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast_control::tests::command_conn_get_status_round_trips_over_tcp -- --nocapture`
Expected: FAIL — `CommandConn` undefined.

- [ ] **Step 3: Write minimal implementation**

Add to `src/audio/snapcast_control.rs`:

```rust
/// A synchronous JSON-RPC command connection to snapserver's control port.
///
/// One request → read lines until the matching `id` response (skipping any
/// interleaved notifications). A `Mutex` serializes callers so request/response
/// pairs never interleave on the socket.
pub struct CommandConn {
    inner: Mutex<ConnInner>,
    next_id: AtomicU64,
}

struct ConnInner {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl CommandConn {
    /// Open a control connection to `host:port` (snapserver's JSON-RPC port).
    pub fn connect(host: &str, port: u16) -> Result<Self, String> {
        let stream = TcpStream::connect((host, port))
            .map_err(|e| format!("snapserver control connect {host}:{port}: {e}"))?;
        let reader = BufReader::new(
            stream.try_clone().map_err(|e| format!("clone control socket: {e}"))?,
        );
        Ok(Self {
            inner: Mutex::new(ConnInner { writer: stream, reader }),
            next_id: AtomicU64::new(1),
        })
    }

    /// Issue one JSON-RPC call and return its `result` value.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let mut guard = self.inner.lock().expect("snapcast control mutex poisoned");
        let mut bytes = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        guard.writer.write_all(&bytes).map_err(|e| format!("control write: {e}"))?;

        // Read lines until the response whose id matches; skip notifications
        // (no `id`) that may interleave.
        loop {
            let mut line = String::new();
            let n = guard.reader.read_line(&mut line).map_err(|e| format!("control read: {e}"))?;
            if n == 0 {
                return Err("snapserver control closed the connection".to_string());
            }
            let msg: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg["id"].as_u64() == Some(id) {
                if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
                    return Err(format!("snapserver error: {err}"));
                }
                return Ok(msg["result"].clone());
            }
            // else: a notification or another id — keep reading.
        }
    }
}

impl SnapcastControl for CommandConn {
    fn get_status(&self) -> Result<ServerStatus, String> {
        let result = self.call("Server.GetStatus", json!({}))?;
        parse_server_status(&result)
    }

    fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String> {
        self.call("Group.SetClients", json!({ "id": group, "clients": clients }))?;
        Ok(())
    }

    fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String> {
        self.call("Group.SetStream", json!({ "id": group, "stream_id": stream }))?;
        Ok(())
    }
}
```

Add the `EventListener` (notification reader). It opens its *own* control connection and invokes `on_event` whenever snapserver pushes a `Client.OnConnect`/`OnDisconnect`/`Server.OnUpdate` notification:

```rust
use std::net::Shutdown;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Listens on a dedicated snapserver control connection for client-(dis)connect
/// notifications and fires `on_event` so the router can reconcile. Uses a second
/// connection (not the command one) so reconcile-issued commands never deadlock
/// against this read loop.
pub struct EventListener {
    stop: Arc<AtomicBool>,
    stream: TcpStream,
    handle: Option<JoinHandle<()>>,
}

impl EventListener {
    pub fn spawn(
        host: &str,
        port: u16,
        on_event: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect((host, port))
            .map_err(|e| format!("snapserver event connect {host}:{port}: {e}"))?;
        let read_stream = stream.try_clone().map_err(|e| format!("clone event socket: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("snapcast-events".to_string())
                .spawn(move || {
                    let mut reader = BufReader::new(read_stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(msg) = serde_json::from_str::<Value>(line.trim()) {
                            if msg["method"].as_str().is_some_and(|m| m.starts_with("Client.")
                                || m == "Server.OnUpdate")
                            {
                                on_event();
                            }
                        }
                    }
                })
                .map_err(|e| format!("spawn event thread: {e}"))?
        };

        Ok(Self { stop, stream, handle: Some(handle) })
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the read loop's blocking read_line.
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
```

(Adjust the existing `use` lines so imports aren't duplicated.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast_control -- --nocapture`
Expected: PASS (parsing + the loopback round-trip). `EventListener` is exercised by the demo gate, not a unit test.

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast_control.rs
git commit -m "Sub-step 3.3: CommandConn JSON-RPC client + EventListener"
```

---

## Sub-step 3.4 — Router + reconciler + engine wiring

### Task 6: Pure `reconcile()` logic

**Files:**
- Modify: `src/audio/snapcast_router.rs` (add `ZoneRouting` + `reconcile`)
- Test: `src/audio/snapcast_router.rs` tests (mock `SnapcastControl`)

**Interfaces:**
- Consumes: `crate::audio::snapcast_control::{SnapcastControl, ServerStatus}`.
- Produces: `pub struct ZoneRouting { pub stream_id: String, pub clients: Vec<String> }`; `pub fn reconcile(control: &dyn SnapcastControl, entries: &[ZoneRouting]) -> Result<(), String>`.

- [ ] **Step 1: Write the failing test**

```rust
    use crate::audio::snapcast_control::{ServerStatus, GroupInfo, SnapcastControl};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockControl {
        status: ServerStatus,
        set_clients: StdMutex<Vec<(String, Vec<String>)>>,
        set_stream: StdMutex<Vec<(String, String)>>,
    }
    impl SnapcastControl for MockControl {
        fn get_status(&self) -> Result<ServerStatus, String> { Ok(self.status.clone()) }
        fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String> {
            self.set_clients.lock().unwrap().push((group.to_string(), clients.to_vec()));
            Ok(())
        }
        fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String> {
            self.set_stream.lock().unwrap().push((group.to_string(), stream.to_string()));
            Ok(())
        }
    }

    fn status_with(groups: Vec<GroupInfo>, connected: &[&str]) -> ServerStatus {
        // ServerStatus.connected is private; build it via parse for the test by
        // round-tripping a GetStatus-shaped value instead.
        let groups_json: Vec<_> = groups.iter().map(|g| serde_json::json!({
            "id": g.id, "stream_id": g.stream_id,
            "clients": g.clients.iter().map(|c| serde_json::json!({
                "id": c, "connected": connected.contains(&c.as_str())
            })).collect::<Vec<_>>()
        })).collect();
        crate::audio::snapcast_control::parse_server_status(
            &serde_json::json!({ "server": { "groups": groups_json } })
        ).unwrap()
    }

    #[test]
    fn reconcile_groups_present_clients_and_binds_stream() {
        let control = MockControl {
            status: status_with(vec![
                GroupInfo { id: "gA".into(), stream_id: "default".into(),
                            clients: vec!["d1".into()] },
                GroupInfo { id: "gB".into(), stream_id: "default".into(),
                            clients: vec!["d2".into()] },
            ], &["d1", "d2"]),
            ..Default::default()
        };
        let entries = vec![ZoneRouting {
            stream_id: "as-0".into(),
            clients: vec!["d1".into(), "d2".into()],
        }];

        reconcile(&control, &entries).expect("reconcile");

        // Both present clients pulled into d1's group, bound to as-0.
        assert_eq!(*control.set_clients.lock().unwrap(),
                   vec![("gA".to_string(), vec!["d1".to_string(), "d2".to_string()])]);
        assert_eq!(*control.set_stream.lock().unwrap(),
                   vec![("gA".to_string(), "as-0".to_string())]);
    }

    #[test]
    fn reconcile_skips_zone_with_no_connected_clients() {
        let control = MockControl {
            status: status_with(vec![
                GroupInfo { id: "gA".into(), stream_id: "default".into(),
                            clients: vec!["d1".into()] },
            ], &[]), // d1 not connected
            ..Default::default()
        };
        let entries = vec![ZoneRouting { stream_id: "as-0".into(), clients: vec!["d1".into()] }];

        reconcile(&control, &entries).expect("reconcile");
        assert!(control.set_clients.lock().unwrap().is_empty());
        assert!(control.set_stream.lock().unwrap().is_empty());
    }
```

(Note: `parse_server_status` must be `pub` — it already is from Task 4. The `connected` field stays private; the test builds status via `parse_server_status`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast_router::tests::reconcile_groups_present_clients_and_binds_stream -- --nocapture`
Expected: FAIL — `ZoneRouting`/`reconcile` undefined.

- [ ] **Step 3: Write minimal implementation**

Add to `src/audio/snapcast_router.rs`:

```rust
use crate::audio::snapcast_control::SnapcastControl;

/// One zone's desired Snapcast routing: the stream its group should play and the
/// dongle client ids that should be in that group.
pub struct ZoneRouting {
    pub stream_id: String,
    pub clients: Vec<String>,
}

/// Converge snapserver's groups/streams to `entries`. Idempotent: re-running with
/// the same desired state is a no-op-equivalent set of calls. For each zone whose
/// clients are (partly) connected, pull the present clients into one group and
/// bind that group to the zone's stream. Zones with no connected client yet are
/// skipped — a later client-connect notification re-triggers reconcile.
pub fn reconcile(control: &dyn SnapcastControl, entries: &[ZoneRouting]) -> Result<(), String> {
    let status = control.get_status()?;
    for entry in entries {
        let present: Vec<String> = entry
            .clients
            .iter()
            .filter(|c| status.is_connected(c))
            .cloned()
            .collect();
        let Some(first) = present.first() else { continue };
        let Some(group) = status.group_of(first) else { continue };
        control.set_group_clients(group, &present)?;
        control.set_group_stream(group, &entry.stream_id)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast_router::tests -- --nocapture`
Expected: PASS (pool + reconcile tests).

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast_router.rs
git commit -m "Sub-step 3.4: pure reconcile() groups present clients onto zone streams"
```

### Task 7: `SnapcastRouter` wiring

**Files:**
- Modify: `src/audio/snapcast_router.rs` (add `SnapcastRouter`)
- Test: `src/audio/snapcast_router.rs` tests (desired-state bookkeeping, device-free)

**Interfaces:**
- Consumes: `StreamPool`, `reconcile`, `ZoneRouting`, `crate::audio::snapcast::SnapserverSupervisor`, `crate::audio::snapcast_control::{CommandConn, EventListener, SnapcastControl}`, `crate::audio::sink::AudioSink`.
- Produces: `SnapcastRouter::new() -> SnapcastRouter`; `fn sink_for_zone(&self, zone: &str, dongle_ids: &[String]) -> Result<Arc<dyn AudioSink>, String>`; `fn release_zone(&self, zone: &str)`; `fn reconcile_now(&self)`. (`new` is I/O-free; `sink_for_zone` lazily starts snapserver + control.)

- [ ] **Step 1: Write the failing test**

Device-free: assert allocation + desired-state bookkeeping without starting snapserver. Inject a started flag so `sink_for_zone` can skip the real `ensure_started` in tests via a test-only constructor that pre-marks "started" with no control.

```rust
    #[test]
    fn router_allocates_and_records_desired_routing() {
        // Test-only: a router whose snapserver/control are considered "absent" so
        // sink_for_zone allocates + records desired state without real I/O.
        let router = SnapcastRouter::for_test();

        let sink = router
            .sink_for_zone("kitchen", &["d1".to_string(), "d2".to_string()])
            .expect("alloc");
        assert_eq!(sink.sample_rate(), 48_000);

        let routing = router.desired_routing_for_test();
        assert_eq!(routing.len(), 1);
        assert_eq!(routing[0].clients, vec!["d1".to_string(), "d2".to_string()]);
        assert!(routing[0].stream_id.starts_with("as-"));

        router.release_zone("kitchen");
        assert!(router.desired_routing_for_test().is_empty());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::snapcast_router::tests::router_allocates_and_records_desired_routing -- --nocapture`
Expected: FAIL — `SnapcastRouter` undefined.

- [ ] **Step 3: Write minimal implementation**

Add to `src/audio/snapcast_router.rs`:

```rust
use std::sync::Mutex;

use crate::audio::snapcast::SnapserverSupervisor;
use crate::audio::snapcast_control::{CommandConn, EventListener, SnapcastControl};

/// Host snapserver's control API listens on (local to the hub).
const CONTROL_HOST: &str = "127.0.0.1";
const CONTROL_PORT: u16 = 1705;

/// The engine's single seam into Snapcast: owns the supervised snapserver, the
/// stream pool, the control connection + event listener, and the desired routing
/// the reconciler converges snapserver to.
pub struct SnapcastRouter {
    started: Mutex<Option<Started>>,
    pool: Mutex<StreamPool>,
    /// zone -> dongle client ids that should be grouped on the zone's stream.
    desired: Mutex<HashMap<String, Vec<String>>>,
}

/// The running side, created lazily on the first `sink_for_zone`.
struct Started {
    _supervisor: SnapserverSupervisor,
    control: Arc<dyn SnapcastControl>,
    _events: EventListener,
}

impl SnapcastRouter {
    pub fn new() -> Self {
        Self {
            started: Mutex::new(None),
            pool: Mutex::new(StreamPool::new(STREAM_POOL_SIZE)),
            desired: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate a stream for `zone`, record its desired grouping, kick a
    /// reconcile, and return the sink to decode into. Lazily starts snapserver +
    /// control on first use. Errors with `no_free_stream` when the pool is full.
    pub fn sink_for_zone(&self, zone: &str, dongle_ids: &[String]) -> Result<Arc<dyn AudioSink>, String> {
        self.ensure_started()?;

        let allocated = {
            let mut pool = self.pool.lock().expect("stream pool mutex poisoned");
            pool.allocate(zone).ok_or_else(|| "no_free_stream".to_string())?
        };

        self.desired
            .lock()
            .expect("desired mutex poisoned")
            .insert(zone.to_string(), dongle_ids.to_vec());

        self.reconcile_now();
        Ok(allocated.sink)
    }

    /// Free `zone`'s stream and drop its desired routing.
    pub fn release_zone(&self, zone: &str) {
        self.pool.lock().expect("stream pool mutex poisoned").release(zone);
        self.desired.lock().expect("desired mutex poisoned").remove(zone);
    }

    /// Build current desired routing from desired state + pool, and reconcile
    /// snapserver to it. Safe to call from the event listener thread.
    pub fn reconcile_now(&self) {
        let control = {
            let guard = self.started.lock().expect("started mutex poisoned");
            match guard.as_ref() {
                Some(s) => Arc::clone(&s.control),
                None => return,
            }
        };
        let entries = self.entries();
        if let Err(e) = reconcile(control.as_ref(), &entries) {
            eprintln!("snapcast reconcile failed (will retry on next trigger): {e}");
        }
    }

    fn entries(&self) -> Vec<ZoneRouting> {
        let desired = self.desired.lock().expect("desired mutex poisoned");
        let pool = self.pool.lock().expect("stream pool mutex poisoned");
        desired
            .iter()
            .filter_map(|(zone, clients)| {
                pool.stream_for(zone).map(|stream_id| ZoneRouting {
                    stream_id,
                    clients: clients.clone(),
                })
            })
            .collect()
    }

    /// Spawn snapserver + open the control connection + start the event listener,
    /// once. Idempotent.
    fn ensure_started(&self) -> Result<(), String> {
        let mut guard = self.started.lock().expect("started mutex poisoned");
        if guard.is_some() {
            return Ok(());
        }
        let supervisor = SnapserverSupervisor::spawn(STREAM_POOL_SIZE)?;
        // Give snapserver a moment to bind its control port before connecting.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let control: Arc<dyn SnapcastControl> = Arc::new(CommandConn::connect(CONTROL_HOST, CONTROL_PORT)?);
        let events = EventListener::spawn(CONTROL_HOST, CONTROL_PORT, || {
            crate::audio::engine::ENGINE.snapcast_on_notify();
        })?;
        *guard = Some(Started { _supervisor: supervisor, control, _events: events });
        Ok(())
    }
}

impl Default for SnapcastRouter {
    fn default() -> Self {
        Self::new()
    }
}
```

Add the test-only helpers behind `#[cfg(test)]` in the same file:

```rust
#[cfg(test)]
impl SnapcastRouter {
    /// A router that behaves as "started" with no real snapserver/control, so
    /// allocation + desired bookkeeping are exercised device-free.
    fn for_test() -> Self {
        Self::new()
    }

    fn desired_routing_for_test(&self) -> Vec<ZoneRouting> {
        self.entries()
    }
}
```

For `for_test`, `sink_for_zone` must not call the real `ensure_started`. Gate it: in `sink_for_zone`, replace `self.ensure_started()?;` with `#[cfg(not(test))] self.ensure_started()?;` so tests skip process/socket startup. (`reconcile_now` already no-ops when `started` is `None`.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::snapcast_router -- --nocapture`
Expected: PASS (pool + reconcile + router bookkeeping). `ensure_started` is demo-gated.

- [ ] **Step 5: Commit**

```bash
git add src/audio/snapcast_router.rs
git commit -m "Sub-step 3.4: SnapcastRouter owns pool, control, events, desired state"
```

### Task 8: Engine routes dongle zones through the router

**Files:**
- Modify: `src/audio/registry.rs` (`Output.sink` → `Option`; update `sink()`, tests)
- Modify: `src/audio/engine.rs` (drop shared `snapcast_sink`; hold `SnapcastRouter`; dongle outputs carry `sink: None`; `zone_sink` routes dongle zones via the router; add `snapcast_on_notify`)
- Test: `src/audio/engine.rs` tests (device-free dongle routing) + `registry.rs` tests updated

**Interfaces:**
- Consumes: `SnapcastRouter::{new, sink_for_zone, release_zone, reconcile_now}`.
- Produces: `Engine::snapcast_on_notify(&self)`; `Output { sink: Option<Arc<dyn AudioSink>>, .. }`; `OutputRegistry::sink` unchanged signature (`Option<Arc<dyn AudioSink>>`) but now also `None` for dongles.

- [ ] **Step 1: Write the failing test**

In `src/audio/registry.rs`, update the test helper to `sink: Some(Arc::new(NullSink))` and add:

```rust
    #[test]
    fn output_without_sink_never_resolves() {
        let registry = OutputRegistry::new();
        registry.register(Output {
            id: "dongle-1".to_string(),
            name: "Kitchen".to_string(),
            sink: None,
            online: true,
        });
        // Registered + online, but no direct sink: dongles are grouped in
        // snapserver, not decoded into individually.
        assert!(registry.sink("dongle-1").is_none());
        assert!(registry.contains("dongle-1"));
    }
```

In `src/audio/engine.rs`, the existing `add_dongle_output_registers_and_creates_zone` asserts `engine.registry.sink("dongle-1").is_some()`. Change that expectation: a dongle output has no direct sink, but its zone exists.

```rust
    #[test]
    fn add_dongle_output_registers_and_creates_zone() {
        let engine = Engine::new();
        engine.add_dongle_output("dongle-1", "Kitchen");

        // A dongle has no direct sink (grouped in snapserver), but is registered…
        assert!(engine.registry.sink("dongle-1").is_none());
        assert!(engine.registry.contains("dongle-1"));
        // …and an auto-zone is created for it.
        let zones = engine.zones.lock().expect("zones");
        let zone = zones.get("dongle-1").expect("auto-zone created");
        assert_eq!(zone.outputs, vec!["dongle-1".to_string()]);
    }
```

Also fix `dongle_offline_unresolves_sink_but_keeps_zone` and `re_register_brings_dongle_back_online`: a dongle's `sink()` is always `None` now, so re-key those on `contains` + `online`. Use `registry.list()` to read the `online` flag:

```rust
    fn dongle_online(engine: &Engine, id: &str) -> Option<bool> {
        engine.registry.list().into_iter().find(|(i, _, _)| i == id).map(|(_, _, on)| on)
    }

    #[test]
    fn dongle_offline_unresolves_sink_but_keeps_zone() {
        let engine = Engine::new();
        engine.add_dongle_output("dongle-1", "Kitchen");
        engine.dongle_offline("dongle-1");

        assert_eq!(dongle_online(&engine, "dongle-1"), Some(false));
        assert!(engine.zones.lock().expect("zones").contains_key("dongle-1"));
    }

    #[test]
    fn re_register_brings_dongle_back_online() {
        let engine = Engine::new();
        engine.add_dongle_output("d", "Name");
        engine.dongle_offline("d");
        assert_eq!(dongle_online(&engine, "d"), Some(false));

        engine.add_dongle_output("d", "Name");
        assert_eq!(dongle_online(&engine, "d"), Some(true));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::registry audio::engine -- --nocapture`
Expected: FAIL to compile — `Output.sink` is still non-optional; engine still references `snapcast_sink`.

- [ ] **Step 3: Write minimal implementation**

In `src/audio/registry.rs`:
- Change the field to `pub sink: Option<Arc<dyn AudioSink>>`.
- Update `sink()`:

```rust
    pub fn sink(&self, id: &str) -> Option<Arc<dyn AudioSink>> {
        let outputs = self.outputs.lock().expect("registry mutex poisoned");
        outputs
            .get(id)
            .filter(|o| o.online)
            .and_then(|o| o.sink.clone())
    }
```
- Update the in-module test helper `output(...)` to set `sink: Some(Arc::new(NullSink))`.

In `src/audio/engine.rs`:
- Remove `snapcast_sink: Arc<dyn AudioSink>` and the `use ... snapcast::{SnapcastSink, ...}` for it; add `use crate::audio::snapcast_router::SnapcastRouter;`.
- Add field `snapcast: SnapcastRouter,` and init `snapcast: SnapcastRouter::new()` in `new()`. Drop the old `snapserver`/`snapcast_sink` fields and `ensure_snapcast` (the router owns snapserver now).
- `register_dongle` becomes:

```rust
    pub fn register_dongle(&self, id: &str, name: &str) -> Result<(), String> {
        self.add_dongle_output(id, name);
        self.notify_outputs_changed();
        // A reconnecting client may already be present; reconcile so it lands on
        // the right stream if its zone is playing.
        self.snapcast.reconcile_now();
        Ok(())
    }
```

- `add_dongle_output` registers with `sink: None`:

```rust
    fn add_dongle_output(&self, id: &str, name: &str) {
        self.registry.register(Output {
            id: id.to_string(),
            name: name.to_string(),
            sink: None,
            online: true,
        });
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones.entry(id.to_string()).or_insert_with(|| ZonePlayback {
            outputs: vec![id.to_string()],
            current: None,
        });
    }
```

- `zone_sink` routes dongle zones via the router. Distinguish local vs dongle by id:

```rust
    fn zone_sink(&self, zone: &str, outputs: &[OutputId]) -> Result<Arc<dyn AudioSink>, String> {
        let has_local = outputs.iter().any(|o| o == LOCAL_OUTPUT_ID);
        let dongle_ids: Vec<String> =
            outputs.iter().filter(|o| *o != LOCAL_OUTPUT_ID).cloned().collect();

        // Mixed zones are rejected at set_zone_outputs; defend here too.
        if has_local && !dongle_ids.is_empty() {
            return Err("mixed_zone_unsupported".to_string());
        }

        if has_local {
            return self.ensure_local();
        }

        // Dongle zone: only online dongles participate.
        let online: Vec<String> = dongle_ids
            .into_iter()
            .filter(|id| self.registry.list().iter().any(|(i, _, on)| i == id && *on))
            .collect();
        if online.is_empty() {
            return Err("zone_has_no_outputs".to_string());
        }
        self.snapcast.sink_for_zone(zone, &online)
    }
```

Update `play` to pass `zone` into `zone_sink` (signature changed) and `stop` to release the router slot:

```rust
        let sink = self.zone_sink(zone, &outputs)?;
```
and in `stop`, after shutting the pipeline:
```rust
    pub fn stop(&self, zone: &str) {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        if let Some(zone_state) = zones.get_mut(zone) {
            if let Some(pipeline) = zone_state.current.take() {
                pipeline.shutdown();
            }
        }
        drop(zones);
        self.snapcast.release_zone(zone);
    }
```

- Add the notify hook used by the event listener:

```rust
    /// Re-run Snapcast reconcile (fired by snapserver client-connect events).
    pub fn snapcast_on_notify(&self) {
        self.snapcast.reconcile_now();
    }
```

- Remove `FanOut`'s now-dead single use? Keep `FanOut` (still referenced by its own test and reserved for local multi-output); `zone_sink`'s single-local path returns `ensure_local()` directly. If the compiler warns `FanOut` is unused outside tests, add `#[allow(dead_code)]` on it with a comment that it's reserved for multi-local-output zones.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::registry audio::engine -- --nocapture`
Expected: PASS. Then `cargo test` (workspace) — all device-free tests green.

- [ ] **Step 5: Commit**

```bash
git add src/audio/registry.rs src/audio/engine.rs
git commit -m "Sub-step 3.4: engine routes dongle zones through SnapcastRouter"
```

---

## Sub-step 3.5 — Zone CRUD + protocol + zones push

### Task 9: Engine zone-CRUD methods + constraints

**Files:**
- Modify: `src/audio/engine.rs` (add `create_zone`/`delete_zone`/`rename_zone`/`set_zone_outputs`/`list_zones`; a `ZonePlayback.name` field)
- Test: `src/audio/engine.rs` tests

**Interfaces:**
- Produces:
  - `Engine::create_zone(&self, name: &str) -> ZoneId`
  - `Engine::delete_zone(&self, zone: &str) -> Result<(), String>`
  - `Engine::rename_zone(&self, zone: &str, name: &str) -> Result<(), String>`
  - `Engine::set_zone_outputs(&self, zone: &str, outputs: &[String]) -> Result<(), String>`
  - `Engine::list_zones(&self) -> Vec<ZoneView>` where `pub struct ZoneView { pub zone: ZoneId, pub name: String, pub outputs: Vec<String>, pub playing: bool }`
- Consumes: `uuid` (already a dependency — used for sessions) for zone ids.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn create_then_set_outputs_and_list() {
        let engine = Engine::new();
        engine.add_dongle_output("d1", "Kitchen");
        engine.add_dongle_output("d2", "Bedroom");

        let zone = engine.create_zone("Upstairs");
        engine.set_zone_outputs(&zone, &["d1".to_string(), "d2".to_string()]).expect("set");

        let view = engine.list_zones().into_iter().find(|z| z.zone == zone).expect("zone listed");
        assert_eq!(view.name, "Upstairs");
        assert_eq!(view.outputs, vec!["d1".to_string(), "d2".to_string()]);
        assert!(!view.playing);
    }

    #[test]
    fn duplicate_names_are_allowed_with_distinct_ids() {
        let engine = Engine::new();
        let a = engine.create_zone("Group");
        let b = engine.create_zone("Group");
        assert_ne!(a, b);
    }

    #[test]
    fn set_zone_outputs_rejects_mixing_local_and_dongle() {
        let engine = Engine::new();
        engine.add_dongle_output("d1", "Kitchen");
        let zone = engine.create_zone("Mix");
        let err = engine
            .set_zone_outputs(&zone, &["local".to_string(), "d1".to_string()])
            .unwrap_err();
        assert_eq!(err, "mixed_zone_unsupported");
    }

    #[test]
    fn set_zone_outputs_rejects_unknown_output() {
        let engine = Engine::new();
        let zone = engine.create_zone("Z");
        let err = engine.set_zone_outputs(&zone, &["ghost".to_string()]).unwrap_err();
        assert_eq!(err, "unknown_output");
    }

    #[test]
    fn set_outputs_unknown_zone_errors() {
        let engine = Engine::new();
        let err = engine.set_zone_outputs("nope", &[]).unwrap_err();
        assert_eq!(err, "unknown_zone");
    }

    #[test]
    fn rename_and_delete_zone() {
        let engine = Engine::new();
        let zone = engine.create_zone("Old");
        engine.rename_zone(&zone, "New").expect("rename");
        assert_eq!(
            engine.list_zones().into_iter().find(|z| z.zone == zone).unwrap().name,
            "New"
        );
        engine.delete_zone(&zone).expect("delete");
        assert!(engine.list_zones().into_iter().all(|z| z.zone != zone));
        assert_eq!(engine.delete_zone(&zone).unwrap_err(), "unknown_zone");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test audio::engine::tests::create_then_set_outputs_and_list -- --nocapture`
Expected: FAIL — methods undefined.

- [ ] **Step 3: Write minimal implementation**

Add a `name` to `ZonePlayback` (so created zones can be labeled) and a default for existing zones:

```rust
struct ZonePlayback {
    name: String,
    outputs: Vec<OutputId>,
    current: Option<Pipeline>,
}
```

Update the two existing `ZonePlayback { .. }` constructions (default zone in `new()`, auto-zone in `add_dongle_output`) to set `name`: the default zone uses `HUB_DISPLAY_NAME`, the dongle auto-zone uses the dongle's `name`.

Add a `ZoneView` and the methods (place near the other `Engine` methods):

```rust
/// A zone as reported to clients: id, label, member output ids, and whether it
/// currently has playback.
pub struct ZoneView {
    pub zone: ZoneId,
    pub name: String,
    pub outputs: Vec<String>,
    pub playing: bool,
}

impl Engine {
    /// Create a new, empty, user-named zone and return its generated id.
    /// Duplicate names are allowed — the id is the identity.
    pub fn create_zone(&self, name: &str) -> ZoneId {
        let id = uuid::Uuid::new_v4().to_string();
        self.zones.lock().expect("engine zones mutex poisoned").insert(
            id.clone(),
            ZonePlayback { name: name.to_string(), outputs: Vec::new(), current: None },
        );
        self.notify_outputs_changed();
        id
    }

    /// Delete a zone, stopping its playback and freeing its Snapcast stream.
    pub fn delete_zone(&self, zone: &str) -> Result<(), String> {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let removed = zones.remove(zone).ok_or_else(|| "unknown_zone".to_string())?;
        drop(zones);
        if let Some(pipeline) = removed.current {
            pipeline.shutdown();
        }
        self.snapcast.release_zone(zone);
        self.notify_outputs_changed();
        Ok(())
    }

    /// Rename a zone's label. Duplicate names are allowed.
    pub fn rename_zone(&self, zone: &str, name: &str) -> Result<(), String> {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let z = zones.get_mut(zone).ok_or_else(|| "unknown_zone".to_string())?;
        z.name = name.to_string();
        drop(zones);
        self.notify_outputs_changed();
        Ok(())
    }

    /// Set a zone's member outputs (the single membership mutator). Enforces that
    /// a zone is all-dongle or all-local, never mixed, and that every id is a
    /// known output.
    pub fn set_zone_outputs(&self, zone: &str, outputs: &[String]) -> Result<(), String> {
        let has_local = outputs.iter().any(|o| o == LOCAL_OUTPUT_ID);
        let has_dongle = outputs.iter().any(|o| o != LOCAL_OUTPUT_ID);
        if has_local && has_dongle {
            return Err("mixed_zone_unsupported".to_string());
        }
        for id in outputs {
            if id != LOCAL_OUTPUT_ID && !self.registry.contains(id) {
                return Err("unknown_output".to_string());
            }
        }
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let z = zones.get_mut(zone).ok_or_else(|| "unknown_zone".to_string())?;
        z.outputs = outputs.to_vec();
        drop(zones);
        self.snapcast.reconcile_now();
        self.notify_outputs_changed();
        Ok(())
    }

    /// Snapshot of all zones for the client `zones` push.
    pub fn list_zones(&self) -> Vec<ZoneView> {
        let zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones
            .iter()
            .map(|(id, z)| ZoneView {
                zone: id.clone(),
                name: z.name.clone(),
                outputs: z.outputs.clone(),
                playing: z.current.is_some(),
            })
            .collect()
    }
}
```

(`uuid` is already used in `session.rs`; confirm `uuid = { features = ["v4"] }` is in `Cargo.toml` — it is, per session UUIDs.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test audio::engine -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/audio/engine.rs
git commit -m "Sub-step 3.5: engine zone CRUD with all-dongle/all-local constraint"
```

### Task 10: Wire tasks for zone CRUD

**Files:**
- Modify: `src/server/commands.rs` (add `CreateZone`/`DeleteZone`/`RenameZone`/`SetZoneOutputs` to `Task`; handlers in `dispatch`; map `no_free_stream`)
- Test: `src/server/commands.rs` tests

**Interfaces:**
- Consumes: `ENGINE.create_zone/delete_zone/rename_zone/set_zone_outputs`.
- Produces: new `Task` variants + wire names `create_zone`/`delete_zone`/`rename_zone`/`set_zone_outputs`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn create_zone_returns_id() {
        let data = serde_json::json!({ "name": "Upstairs" });
        let json = dispatch(Task::parse("create_zone"), &data).to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"task\":\"create_zone\""));
        assert!(json.contains("\"zone\":\""));
    }

    #[test]
    fn delete_unknown_zone_errors() {
        let data = serde_json::json!({ "zone": "ghost" });
        let json = dispatch(Task::parse("delete_zone"), &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_zone\""));
    }

    #[test]
    fn set_zone_outputs_unknown_output_errors() {
        // Target the always-present default zone with a non-existent output.
        let data = serde_json::json!({ "zone": "default", "outputs": ["ghost"] });
        let json = dispatch(Task::parse("set_zone_outputs"), &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_output\""));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test server::commands::tests::create_zone_returns_id -- --nocapture`
Expected: FAIL — unknown tasks resolve to `unsupported_task`.

- [ ] **Step 3: Write minimal implementation**

In `src/server/commands.rs`, add variants to `Task`, to `parse`, and to `name`:

```rust
    CreateZone,
    DeleteZone,
    RenameZone,
    SetZoneOutputs,
```
```rust
            "create_zone" => Task::CreateZone,
            "delete_zone" => Task::DeleteZone,
            "rename_zone" => Task::RenameZone,
            "set_zone_outputs" => Task::SetZoneOutputs,
```
```rust
            Task::CreateZone => "create_zone",
            Task::DeleteZone => "delete_zone",
            Task::RenameZone => "rename_zone",
            Task::SetZoneOutputs => "set_zone_outputs",
```

Add `no_free_stream` to the `play` error mapping:

```rust
                    let code = match e.as_str() {
                        "unknown_zone" => "unknown_zone",
                        "zone_has_no_outputs" => "zone_has_no_outputs",
                        "no_free_stream" => "no_free_stream",
                        "mixed_zone_unsupported" => "mixed_zone_unsupported",
                        _ => "playback_failed",
                    };
```

Add handler arms in `dispatch` (before the `Task::Unknown` arm). Each reads its fields and maps engine `Result`s to responses:

```rust
        Task::CreateZone => {
            let name = data["name"].as_str().unwrap_or("Zone");
            let id = ENGINE.create_zone(name);
            TaskResponse::accepted("create_zone", Some(json!({ "zone": id })))
        }
        Task::DeleteZone => match data["zone"].as_str() {
            Some(zone) if !zone.is_empty() => match ENGINE.delete_zone(zone) {
                Ok(()) => TaskResponse::accepted("delete_zone", None),
                Err(code) => TaskResponse::error("delete_zone", code),
            },
            _ => TaskResponse::error("delete_zone", "unknown_zone"),
        },
        Task::RenameZone => match (data["zone"].as_str(), data["name"].as_str()) {
            (Some(zone), Some(name)) if !zone.is_empty() => match ENGINE.rename_zone(zone, name) {
                Ok(()) => TaskResponse::accepted("rename_zone", None),
                Err(code) => TaskResponse::error("rename_zone", code),
            },
            _ => TaskResponse::error("rename_zone", "unknown_zone"),
        },
        Task::SetZoneOutputs => {
            let zone = data["zone"].as_str().unwrap_or("");
            let outputs: Vec<String> = data["outputs"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if zone.is_empty() {
                TaskResponse::error("set_zone_outputs", "unknown_zone")
            } else {
                match ENGINE.set_zone_outputs(zone, &outputs) {
                    Ok(()) => TaskResponse::accepted("set_zone_outputs", None),
                    Err(code) => TaskResponse::error("set_zone_outputs", code),
                }
            }
        }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test server::commands -- --nocapture`
Expected: PASS (new + existing arms). Note these tests touch the global `ENGINE`; `create_zone`/`set_zone_outputs` are device-free (no audio).

- [ ] **Step 5: Commit**

```bash
git add src/server/commands.rs
git commit -m "Sub-step 3.5: wire tasks for zone CRUD + no_free_stream error"
```

### Task 11: `zones` live push + `list_zones` pull

**Files:**
- Modify: `src/server/connection.rs` (push `zones` next to `outputs`; handle `list_zones`)
- Test: covered by existing connection behavior + manual; add no new unit test (the encrypted connection isn't unit-tested today — keep parity).

**Interfaces:**
- Consumes: `ENGINE.list_zones() -> Vec<ZoneView>`.
- Produces: a `{ "task": "zones", "data": { "zones": [...] } }` push; `list_zones` pull handled like `list_outputs`.

- [ ] **Step 1: Add the `zones` sender**

In `src/server/connection.rs`, add alongside `send_outputs`:

```rust
    /// Push the current zone definitions (id, name, member outputs, playing) so a
    /// grouping UI can render membership. Additive to the flat `outputs` push.
    async fn send_zones(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let zones: Vec<serde_json::Value> = ENGINE
            .list_zones()
            .into_iter()
            .map(|z| json!({
                "zone": z.zone, "name": z.name, "outputs": z.outputs, "playing": z.playing
            }))
            .collect();
        let response = TaskResponse::accepted("zones", Some(json!({ "zones": zones })));
        self.send_encrypted(&response.to_json()).await
    }
```

- [ ] **Step 2: Push zones on connect + on change + on demand**

Where `send_outputs` is called on connect and on the `OUTPUTS_CHANGED` tick, also call `send_zones`. In the connect path:

```rust
        if self.send_outputs().await.is_err() {
            return; // or existing error handling
        }
        if self.send_zones().await.is_err() {
            return;
        }
```

In the `outputs_changed.recv()` select arm, after re-pushing outputs:

```rust
                _ = outputs_changed.recv() => {
                    if self.send_outputs().await.is_err() { /* existing */ }
                    if self.send_zones().await.is_err() { /* existing */ }
                }
```

In `handle_task`, add a `list_zones` pull beside `list_outputs`:

```rust
            Some("list_outputs") => return self.send_outputs().await,
            Some("list_zones") => return self.send_zones().await,
```

(Match the file's existing error-handling style for the `send_*` calls — mirror exactly what the current `send_outputs` arms do.)

- [ ] **Step 3: Build to verify**

Run: `cargo build`
Expected: compiles. Run `cargo test` — all device-free tests stay green.

- [ ] **Step 4: Commit**

```bash
git add src/server/connection.rs
git commit -m "Sub-step 3.5: push zones list to clients (live + list_zones pull)"
```

### Task 12: Documentation

**Files:**
- Modify: `CLAUDE.md` (protocol section + current-state paragraph)
- Modify: `docs/multi-room-plan.md` (mark sub-step 3 landed; retire the manual Group.SetStream bring-up workaround)

- [ ] **Step 1: Update `CLAUDE.md`**

In the wire-protocol section, add the new tasks to the recognized list and document them, the new `play` errors (`no_free_stream`, `mixed_zone_unsupported`), and the `zones` push (with the JSON example from the spec §5). Mark each addition **"hub-side shipped, iOS mirror pending (own spec)."** In the current-state paragraph, replace "all dongles share one snapserver stream (one synced group)" with the sub-step 3 reality: per-zone streams from a `SnapcastRouter` + desired-state reconciler over snapserver JSON-RPC; `Output.sink` is now `Option` (dongles `None`).

- [ ] **Step 2: Update `docs/multi-room-plan.md`**

Add a "Sub-step 3 — landed" subsection at the same detail level as the 2.x entries: the `SnapcastRouter`/pool/reconciler design, the `SnapcastSink` backpressure fix, zone CRUD + protocol, and what stays deferred (iOS grouping UI; auth on dongle channels). In bring-up note #2, mark the manual `Group.SetStream` workaround **retired** (now automated by the reconciler).

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md docs/multi-room-plan.md
git commit -m "Sub-step 3.5: document streams/grouping protocol + plan status"
```

---

## Final verification

- [ ] Run the full device-free suite: `cargo test` (workspace root). Expected: all green, no hardware/binaries needed.
- [ ] `cargo build --release` (and `./compile.sh` for the Pi target if available) compiles clean.
- [ ] Demo gate (manual, needs `snapserver` + 2× `snapclient` + audio): start the hub, register two dongles, `play {zone:<d1>}` and `play {zone:<d2>}` with different URLs → confirm **independent** audio. Then `create_zone` + `set_zone_outputs` grouping both, `play {zone:<group>}` → confirm **synchronized** audio on both. Confirm a late-joining dongle is pulled onto the right stream without manual RPC.

---

## Self-Review notes (already reconciled against the spec)

- **Spec §3.1 backpressure** → Task 1. **§3.2 multi-stream + pool** → Tasks 2–3. **§3.3 control** → Tasks 4–5. **§3.4 router/reconciler/engine** → Tasks 6–8. **§4 zone CRUD + constraints** → Task 9. **§5 protocol + zones push** → Tasks 10–11. **§8 testing** → device-free tests in each task + the demo gate. **§10 docs** → Task 12.
- **Type consistency:** `SnapcastControl` (Tasks 4–7), `ServerStatus`/`GroupInfo`/`parse_server_status` (Tasks 4, 6), `ZoneRouting`/`reconcile` (Tasks 6–7), `AllocatedStream`/`StreamPool` (Tasks 3, 7), `ZoneView` (Tasks 9, 11), `Output.sink: Option<…>` (Task 8 onward) are used consistently across tasks.
- **Deferred (not in this plan, per spec non-goals):** iOS grouping UI; auth on dongle channels; local-output jitter buffer (KAN-23 base).
