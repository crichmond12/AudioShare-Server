//! Snapcast output path (multi-room Change 5, sub-step 1).
//!
//! This is the hub's first *real* second output and the entry point for
//! **synchronized** multi-room (roadmap Phase 3). Per the architecture plan
//! (`docs/multi-room-plan.md`, Change 5), Snapcast is an **implementation
//! detail behind a seam**: the engine only ever sees an [`AudioSink`], and the
//! grouped zone's sink becomes a [`SnapcastSink`] that feeds a supervised
//! `snapserver`. Dongles run `snapclient`, which does the sub-millisecond clock
//! alignment we deliberately do not hand-roll.
//!
//! Two pieces live here, the building blocks of sub-step 1:
//! - [`SnapcastSink`] — `impl AudioSink`; converts the decode pipeline's
//!   interleaved `f32` PCM to the `s16le` `snapserver` expects and writes it to
//!   snapserver's input FIFO.
//! - [`SnapserverSupervisor`] — spawns and restarts a `snapserver` process
//!   configured with a single pipe stream reading that FIFO.
//!
//! Sub-step 1 is verified by hand against a stock `snapclient` on a laptop (see
//! the ignored `plays_to_snapcast_briefly` test); the custom dongle agent,
//! registration, and hub-driven grouping are later sub-steps and do not touch
//! this file's contract.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::audio::sink::AudioSink;

/// The fixed PCM format the snapserver pipe stream is configured for. snapserver
/// reads raw interleaved `s16le` at a single declared sample format; the decode
/// pipeline resamples/mixes to whatever the sink reports, so reporting these
/// makes the two agree without negotiation. (Per-output format negotiation is a
/// later Change 5 concern; for now every Snapcast output shares this canonical
/// CD-ish stereo format.)
const SNAPCAST_SAMPLE_RATE: u32 = 48_000;
const SNAPCAST_CHANNELS: u16 = 2;
/// `snapserver`'s `sampleformat` string for the above (`rate:bits:channels`).
const SNAPCAST_SAMPLE_FORMAT: &str = "48000:16:2";

/// Default path for the FIFO snapserver reads and the sink writes. snapserver
/// owns it (created via `mode=create`); the sink is just the writer.
pub const DEFAULT_FIFO_PATH: &str = "/tmp/audioshare-snapfifo";

/// How long the supervisor waits before relaunching a `snapserver` that exited.
const SNAPSERVER_RESTART_DELAY: Duration = Duration::from_secs(1);

/// An [`AudioSink`] that writes a zone's PCM into a `snapserver` input FIFO.
///
/// The decode pipeline writes interleaved `f32`; this converts each sample to
/// `s16le` and writes it to the FIFO. The FIFO write end is opened lazily and
/// non-blocking: until `snapserver` has the read end open the open fails with
/// `ENXIO`, so we simply drop that buffer and retry on the next `write` (the
/// decode thread never blocks on snapserver coming up). A full pipe buffer
/// (`WouldBlock`) likewise drops the overflow — matching [`crate::audio::output`]'s
/// "drop oldest on overrun" stance; proper jitter buffering remains KAN-23. Any
/// other write error (e.g. the reader vanished on a snapserver restart) drops
/// the handle so the next `write` reopens it.
pub struct SnapcastSink {
    fifo_path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The FIFO write end, opened lazily once snapserver is reading.
    writer: Option<File>,
    /// Reused `s16le` conversion buffer to avoid per-write allocation.
    scratch: Vec<u8>,
}

impl SnapcastSink {
    /// Create a sink writing to `fifo_path`. Does no I/O — the FIFO is opened on
    /// the first [`write`](AudioSink::write) once snapserver is reading it.
    pub fn new(fifo_path: impl Into<PathBuf>) -> Self {
        Self {
            fifo_path: fifo_path.into(),
            inner: Mutex::new(Inner {
                writer: None,
                scratch: Vec::new(),
            }),
        }
    }
}

impl AudioSink for SnapcastSink {
    fn sample_rate(&self) -> u32 {
        SNAPCAST_SAMPLE_RATE
    }

    fn channels(&self) -> u16 {
        SNAPCAST_CHANNELS
    }

    fn write(&self, samples: &[f32]) {
        let inner = &mut *self.inner.lock().expect("snapcast sink mutex poisoned");

        // Lazily (re)open the FIFO write end. ENXIO ("no reader") means
        // snapserver isn't up yet — drop this buffer and try again next time.
        if inner.writer.is_none() {
            match open_fifo_write(&self.fifo_path) {
                Ok(file) => inner.writer = Some(file),
                Err(_) => return,
            }
        }

        inner.scratch.clear();
        to_i16le(samples, &mut inner.scratch);

        // Disjoint field borrows: the writer and the scratch buffer.
        let Some(file) = inner.writer.as_mut() else {
            return;
        };
        match file.write(&inner.scratch) {
            // A short write means the pipe buffer filled; we drop the overflow
            // rather than block the decode thread.
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            // Reader gone / broken pipe: drop the handle and reopen next write.
            Err(_) => inner.writer = None,
        }
    }
}

/// Open the write end of an existing FIFO without blocking on a reader.
///
/// A blocking open of a FIFO for writing waits until a reader appears; with
/// `O_NONBLOCK` it instead returns `ENXIO` immediately when there is no reader,
/// which lets the sink poll for snapserver coming up instead of stalling the
/// decode thread.
fn open_fifo_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// Convert interleaved `f32` samples in `[-1.0, 1.0]` to little-endian `s16`,
/// appending to `out`. Out-of-range samples are clamped before scaling.
fn to_i16le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for &s in samples {
        let scaled = s.clamp(-1.0, 1.0) * i16::MAX as f32;
        // Round to nearest to minimize quantization bias.
        let v = scaled.round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Supervises a `snapserver` process configured with a single pipe stream that
/// reads [`SnapcastSink`]'s FIFO.
///
/// The plan keeps Snapcast a swappable implementation detail, so this stays a
/// thin supervisor: it spawns `snapserver`, relaunches it if it exits, and kills
/// it on drop. It does **not** speak snapserver's JSON-RPC API — hub-driven
/// group/stream programming is a later sub-step. For sub-step 1 the static
/// single-stream config is all that's needed to prove the audio + sync path.
pub struct SnapserverSupervisor {
    stop: Arc<AtomicBool>,
    /// Shared so [`Drop`] can kill the live child immediately rather than wait
    /// out the monitor loop's next iteration.
    child: Arc<Mutex<Option<Child>>>,
    monitor: Option<JoinHandle<()>>,
}

impl SnapserverSupervisor {
    /// Spawn `snapserver` (resolved from `PATH`) reading the FIFO at
    /// [`DEFAULT_FIFO_PATH`]. See [`spawn_with`](Self::spawn_with) to override.
    pub fn spawn() -> Result<Self, String> {
        Self::spawn_with("snapserver", DEFAULT_FIFO_PATH)
    }

    /// Spawn `binary` as the snapserver, configured with one pipe stream named
    /// `AudioShare` reading `fifo_path` (which snapserver creates via
    /// `mode=create`). Returns an error only if the *first* launch fails to
    /// spawn — later crashes are handled by the restart loop.
    pub fn spawn_with(
        binary: impl Into<String>,
        fifo_path: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let binary = binary.into();
        let fifo_path = fifo_path.into();
        let source = pipe_source(&fifo_path);

        // Launch once up front so a misconfiguration (missing binary) surfaces
        // to the caller instead of being silently retried forever.
        let first = spawn_snapserver(&binary, &source)?;

        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(Some(first)));

        let monitor = {
            let stop = Arc::clone(&stop);
            let child = Arc::clone(&child);
            thread::Builder::new()
                .name("snapserver-supervisor".to_string())
                .spawn(move || monitor_loop(&binary, &source, &stop, &child))
                .map_err(|e| format!("failed to spawn snapserver supervisor thread: {e}"))?
        };

        Ok(Self {
            stop,
            child,
            monitor: Some(monitor),
        })
    }
}

impl Drop for SnapserverSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Kill the current child so the monitor's `wait` returns promptly; with
        // `stop` set it will then exit instead of relaunching.
        if let Some(mut child) = self
            .child
            .lock()
            .expect("snapserver child mutex poisoned")
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

/// The monitor loop: wait on the current child, and while not stopping, relaunch
/// it after a short delay if it exits.
fn monitor_loop(
    binary: &str,
    source: &str,
    stop: &AtomicBool,
    child: &Mutex<Option<Child>>,
) {
    loop {
        // Take the live child to wait on it outside the lock (so Drop can still
        // observe/kill via the slot on the next launch).
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

        match spawn_snapserver(binary, source) {
            Ok(next) => *child.lock().expect("snapserver child mutex poisoned") = Some(next),
            Err(e) => {
                eprintln!("snapserver relaunch failed: {e}");
                return;
            }
        }
    }
}

/// Build the `snapserver` pipe-stream source URI for `fifo_path`: a PCM pipe
/// stream named `AudioShare` at the canonical sample format, with snapserver
/// owning (creating) the FIFO.
fn pipe_source(fifo_path: &Path) -> String {
    format!(
        "pipe://{}?name=AudioShare&mode=create&sampleformat={}&codec=pcm",
        fifo_path.display(),
        SNAPCAST_SAMPLE_FORMAT
    )
}

/// Spawn one `snapserver` process with the given stream source.
fn spawn_snapserver(binary: &str, source: &str) -> Result<Child, String> {
    Command::new(binary)
        .arg("--stream.source")
        .arg(source)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn snapserver `{binary}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapcast_sink_reports_canonical_format() {
        let sink = SnapcastSink::new("/tmp/does-not-matter");
        assert_eq!(sink.sample_rate(), 48_000);
        assert_eq!(sink.channels(), 2);
    }

    #[test]
    fn to_i16le_scales_and_clamps() {
        let mut out = Vec::new();
        // 0.0 -> 0, +1.0 -> i16::MAX, -1.0 -> -i16::MAX, and out-of-range clamps.
        to_i16le(&[0.0, 1.0, -1.0, 2.0, -2.0], &mut out);

        let decoded: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(decoded, vec![0, i16::MAX, -i16::MAX, i16::MAX, -i16::MAX]);
    }

    #[test]
    fn to_i16le_emits_two_bytes_per_sample() {
        let mut out = Vec::new();
        to_i16le(&[0.1, -0.1, 0.5], &mut out);
        assert_eq!(out.len(), 3 * 2);
    }

    #[test]
    fn write_without_reader_is_a_silent_drop() {
        // No snapserver, no FIFO: opening the write end fails, so write must be a
        // no-op rather than panic or block.
        let sink = SnapcastSink::new("/tmp/audioshare-nonexistent-fifo-for-test");
        sink.write(&[0.0; 8]);
        assert!(sink
            .inner
            .lock()
            .expect("mutex")
            .writer
            .is_none());
    }

    #[test]
    fn pipe_source_encodes_format_and_create_mode() {
        let source = pipe_source(Path::new("/tmp/snapfifo"));
        assert!(source.contains("pipe:///tmp/snapfifo"));
        assert!(source.contains("mode=create"));
        assert!(source.contains("sampleformat=48000:16:2"));
        assert!(source.contains("codec=pcm"));
    }

    // Live end-to-end check (opt-in: needs the `snapserver`/`snapclient` binaries,
    // network, and audio hardware). Proves sub-step 1's audio + sync path with a
    // stock snapclient and zero custom dongle code:
    //   cargo test audio::snapcast::tests::plays_to_snapcast_briefly -- --ignored --nocapture
    // While it runs, on the same or another machine:  snapclient -h <this-host>
    // You should hear ~5s of SomaFM, clock-synced by Snapcast.
    #[test]
    #[ignore]
    fn plays_to_snapcast_briefly() {
        use crate::audio::decode;
        use std::sync::atomic::AtomicBool;

        const URL: &str = "https://ice1.somafm.com/groovesalad-128-mp3";

        let _server =
            SnapserverSupervisor::spawn().expect("snapserver should spawn (is it installed?)");
        // Give snapserver a moment to create the FIFO and open the read end.
        thread::sleep(Duration::from_secs(1));

        let sink = SnapcastSink::new(DEFAULT_FIFO_PATH);
        let stop = Arc::new(AtomicBool::new(false));

        let stop_for_thread = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            decode::stream_url_to_output(URL, &sink, &stop_for_thread)
        });

        thread::sleep(Duration::from_secs(5));
        stop.store(true, Ordering::Relaxed);

        let result = worker.join().expect("decode thread panicked");
        assert!(result.is_ok(), "playback errored: {result:?}");
    }
}
