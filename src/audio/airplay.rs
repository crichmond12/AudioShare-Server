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

use std::ffi::CString;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::audio::decode::{mix_planar, ResamplePipeline};
use crate::audio::sink::AudioSink;

/// AirPlay always delivers CD audio: 44.1 kHz, 16-bit, stereo.
pub const AIRPLAY_SAMPLE_RATE: u32 = 44_100;
pub const AIRPLAY_CHANNELS: usize = 2;

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

    #[test]
    fn pump_passes_through_at_native_rate() {
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
}
