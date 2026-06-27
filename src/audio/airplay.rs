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

/// Base RTSP port for classic shairport-sync instances; instance `slot` uses
/// `RTSP_PORT_BASE + slot`. Classic AirPlay needs a distinct port per instance.
pub const RTSP_PORT_BASE: u16 = 5055;

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

/// Path of the audio FIFO backing receiver `index`.
pub fn fifo_path(index: usize) -> PathBuf {
    PathBuf::from(format!("{FIFO_PATH_BASE}-{index}.pcm"))
}

/// Path of the metadata FIFO backing receiver `index` (parallel to [`fifo_path`]).
pub fn meta_fifo_path(index: usize) -> PathBuf {
    PathBuf::from(format!("{FIFO_PATH_BASE}-{index}.meta"))
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
/// receiver on `port` whose `pipe` backend writes raw PCM to `fifo_path`, plus a
/// `metadata` backend writing the DAAP/PICT metadata stream to a sibling `.meta`
/// FIFO (same stem as the audio FIFO).
fn shairport_config(name: &str, port: u16, device_id: &str, fifo_path: &Path) -> String {
    let meta_path = fifo_path.with_extension("meta");
    format!(
        "general =\n{{\n  name = \"{name}\";\n  port = {port};\n  airplay_device_id = \"{device_id}\";\n}};\n\n\
         pipe =\n{{\n  name = \"{}\";\n}};\n\n\
         metadata =\n{{\n  enabled = \"yes\";\n  include_cover_art = \"yes\";\n  pipe_name = \"{}\";\n}};\n",
        fifo_path.display(),
        meta_path.display(),
    )
}

/// Create the FIFO at `path` if it does not already exist (mode 0o600).
pub(crate) fn ensure_fifo(path: &Path) -> Result<(), String> {
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
    /// on `port`, writing PCM to `fifo`, with device id `device_id`.
    pub fn spawn(name: &str, port: u16, device_id: &str, fifo: &Path) -> Result<Self, String> {
        Self::spawn_with("shairport-sync", name, port, device_id, fifo)
    }

    /// Production entry: spawn the receiver for `slot`, deriving port, device id,
    /// and audio FIFO from the slot index.
    pub fn spawn_for_slot(name: &str, slot: usize) -> Result<Self, String> {
        Self::spawn(name, slot_port(slot), &slot_device_id(slot), &fifo_path(slot))
    }

    /// Like [`spawn`](Self::spawn) but with an explicit binary (for tests/dev).
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
        let cfg = shairport_config("Audio Share (Hub)", 5000, "AA5500000000", Path::new("/tmp/x.pcm"));
        assert!(cfg.contains("name = \"Audio Share (Hub)\""), "{cfg}");
        assert!(cfg.contains("port = 5000"), "{cfg}");
        assert!(cfg.contains("name = \"/tmp/x.pcm\""), "{cfg}"); // pipe.name
        assert!(cfg.contains("pipe ="), "{cfg}");
    }

    #[test]
    fn slot_maps_to_unique_port_and_device_id() {
        assert_eq!(slot_port(0), RTSP_PORT_BASE);
        assert_eq!(slot_port(3), RTSP_PORT_BASE + 3);
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

    #[test]
    fn fifo_path_is_indexed() {
        assert_eq!(fifo_path(0), PathBuf::from("/tmp/audioshare-airplay-0.pcm"));
        assert_eq!(fifo_path(3), PathBuf::from("/tmp/audioshare-airplay-3.pcm"));
    }

    #[test]
    fn meta_fifo_path_is_indexed() {
        assert_eq!(meta_fifo_path(0), PathBuf::from("/tmp/audioshare-airplay-0.meta"));
        assert_eq!(meta_fifo_path(3), PathBuf::from("/tmp/audioshare-airplay-3.meta"));
    }

    #[test]
    fn config_enables_metadata_pipe() {
        let cfg = shairport_config("Kitchen", 5002, "AA5500000002", Path::new("/tmp/x.pcm"));
        assert!(cfg.contains("metadata ="), "{cfg}");
        assert!(cfg.contains("enabled = \"yes\""), "{cfg}");
        assert!(cfg.contains("include_cover_art = \"yes\""), "{cfg}");
        assert!(cfg.contains("/tmp/x.meta"), "{cfg}"); // meta pipe path derived from audio path
    }

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

        let _server = ShairportSupervisor::spawn_for_slot("Audio Share (Hub)", 0)
            .expect("shairport-sync should spawn (is the classic build installed?)");

        let output = Arc::new(AudioOutput::new().expect("open default output device"));
        let stop = Arc::new(AtomicBool::new(false));

        let sink: Arc<dyn AudioSink> = output.clone();
        let pump_stop = Arc::clone(&stop);
        let pump_fifo = fifo_path(0);
        let worker = thread::spawn(move || {
            pump_fifo_to_sink(&pump_fifo, &sink, &pump_stop)
        });

        thread::sleep(Duration::from_secs(30));
        stop.store(true, Ordering::Relaxed);
        // The pump may be parked in a blocking FIFO read/open; this test does not
        // join it (the process exits at test end). Stop responsiveness during an
        // active read is a Slice 2 concern (session tracking via the metadata pipe).
        drop(worker);
    }
}
