//! Playback engine (multi-room Changes 2 + 3; was `player.rs`).
//!
//! [`Engine`] owns the [`OutputRegistry`] and one in-flight decode pipeline
//! *per zone*. A **zone** is a named group of outputs that share playback;
//! `play(zone, url)` streams a URL to that zone's outputs and `stop(zone)`
//! halts just that zone. This replaces the single-stream `Player`: the engine
//! can now drive several zones independently (the headline multi-room feature).
//!
//! For this step there is one `"default"` zone targeting the single `"local"`
//! output, so externally observable behavior matches the old single-zone
//! engine. Reading the target zone off the wire is Change 4; real second
//! outputs (network sinks / dongles) are Change 5.
//!
//! A process-wide [`ENGINE`] is exposed so `commands::dispatch` can reach it
//! without threading a handle through every `Connection`, mirroring the
//! `MAIN_SERVER` global in `server::server`. Critically, constructing `ENGINE`
//! does **not** open the audio device — the local device is opened lazily on
//! first `play` so device-free paths (`stop`, tests) never need hardware.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use lazy_static::lazy_static;
use tokio::sync::broadcast;

use crate::audio::decode;
use crate::audio::output::AudioOutput;
use crate::audio::registry::{Output, OutputId, OutputRegistry};
use crate::audio::sink::AudioSink;
use crate::audio::snapcast::{self, SnapcastSink, SnapserverSupervisor};

/// Reserved id for the host's own cpal output device.
const LOCAL_OUTPUT_ID: &str = "local";
/// Zone used until the protocol carries a target zone (Change 4).
const DEFAULT_ZONE: &str = "default";
/// Display name reported for the hub's own (local) output in the target list.
const HUB_DISPLAY_NAME: &str = "Hub";

lazy_static! {
    /// Process-wide playback engine used by `commands::dispatch`.
    pub static ref ENGINE: Engine = Engine::new();

    /// Broadcast tick fired whenever the set/state of outputs changes (a dongle
    /// attaches or drops). Per-client `Connection`s subscribe and re-push the
    /// current target list (`list_targets`) so the iOS speaker picker stays live.
    /// Carries no payload — subscribers always re-query the full snapshot, so a
    /// missed/lagged tick is harmless. The registry stays observer-free (per its
    /// own doc); the engine owns this eventing.
    pub static ref OUTPUTS_CHANGED: broadcast::Sender<()> = broadcast::channel(16).0;
}

/// Name of a zone (a group of outputs sharing playback).
pub type ZoneId = String;

/// A running decode pipeline: its cooperative stop flag and the thread driving it.
struct Pipeline {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Pipeline {
    /// Signal the decode thread to stop and wait for it to exit.
    fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

/// A zone's membership plus its current in-flight playback (if any).
struct ZonePlayback {
    outputs: Vec<OutputId>,
    current: Option<Pipeline>,
}

/// The multi-room playback engine. Shared process-wide via [`ENGINE`].
pub struct Engine {
    registry: Arc<OutputRegistry>,
    zones: Mutex<HashMap<ZoneId, ZonePlayback>>,
    /// The supervised `snapserver`, spawned lazily on the first dongle
    /// registration (see [`ensure_snapcast`](Self::ensure_snapcast)). `None`
    /// until then so construction and the local-only path touch no process.
    snapserver: Mutex<Option<SnapserverSupervisor>>,
    /// The single shared sink feeding `snapserver`'s one input stream. Every
    /// registered dongle's output points at *this* sink, so in sub-step 2 all
    /// dongles are snapclients of one stream and play the **same** audio (one
    /// synchronized group). Per-dongle independent streams/grouping is sub-step 3
    /// (snapserver JSON-RPC); until then, routing two zones to two dongles at once
    /// would interleave both into this one FIFO and garble — by design, not a bug.
    /// Constructing it does no I/O, so this is safe to build eagerly.
    snapcast_sink: Arc<dyn AudioSink>,
}

impl Engine {
    pub fn new() -> Self {
        // One default zone targeting the local device. Note: this does NOT open
        // the device — `ensure_local` does that lazily on first play.
        let mut zones = HashMap::new();
        zones.insert(
            DEFAULT_ZONE.to_string(),
            ZonePlayback {
                outputs: vec![LOCAL_OUTPUT_ID.to_string()],
                current: None,
            },
        );
        Self {
            registry: Arc::new(OutputRegistry::new()),
            zones: Mutex::new(zones),
            snapserver: Mutex::new(None),
            // I/O-free: the FIFO is opened lazily on first write once snapserver
            // is reading it (see `SnapcastSink`).
            snapcast_sink: Arc::new(SnapcastSink::new(snapcast::fifo_path(0))),
        }
    }

    /// Open the host's cpal device if it isn't already, register it as the
    /// `"local"` output, and return its sink. Idempotent and the only place the
    /// audio device is acquired — so the device-open error surfaces here (as
    /// today's `playback_failed`) rather than at construction.
    fn ensure_local(&self) -> Result<Arc<dyn AudioSink>, String> {
        if let Some(sink) = self.registry.sink(LOCAL_OUTPUT_ID) {
            return Ok(sink);
        }
        let sink: Arc<dyn AudioSink> = Arc::new(AudioOutput::new()?);
        self.registry.register(Output {
            id: LOCAL_OUTPUT_ID.to_string(),
            name: "Local".to_string(),
            sink: Arc::clone(&sink),
            online: true,
        });
        Ok(sink)
    }

    /// Resolve a zone's online outputs to a single sink to decode into: the lone
    /// sink directly when there's exactly one (preserving its native rate), or a
    /// [`FanOut`] over several. Errors if the zone is unknown or has no
    /// reachable outputs.
    fn zone_sink(&self, outputs: &[OutputId]) -> Result<Arc<dyn AudioSink>, String> {
        // The local device is opened on demand; other outputs (dongles) must
        // already be registered + online to participate.
        if outputs.iter().any(|o| o == LOCAL_OUTPUT_ID) {
            self.ensure_local()?;
        }

        let mut sinks: Vec<Arc<dyn AudioSink>> = outputs
            .iter()
            .filter_map(|id| self.registry.sink(id))
            .collect();

        match sinks.len() {
            0 => Err("zone_has_no_outputs".to_string()),
            // Single sink: pass through directly so decode resamples to the
            // device's own rate. Wrapping in FanOut would force a canonical rate
            // the device may not run at.
            1 => Ok(sinks.pop().expect("len checked")),
            _ => Ok(Arc::new(FanOut::new(sinks))),
        }
    }

    /// Start streaming `url` to `zone`, replacing that zone's current playback.
    /// Other zones are unaffected. Returns an error if the zone is unknown, has
    /// no reachable outputs, or the local audio device can't be opened; later
    /// stream/decode failures surface on the decode thread and simply end
    /// playback.
    pub fn play(&self, zone: &str, url: &str) -> Result<(), String> {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let outputs = zones
            .get(zone)
            .map(|z| z.outputs.clone())
            .ok_or_else(|| "unknown_zone".to_string())?;

        let sink = self.zone_sink(&outputs)?;

        // Stop this zone's existing playback before starting the new stream.
        let zone_state = zones.get_mut(zone).expect("zone existed above");
        if let Some(pipeline) = zone_state.current.take() {
            pipeline.shutdown();
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_url = url.to_string();

        let handle = thread::Builder::new()
            .name(format!("decode-{zone}"))
            .spawn(move || {
                if let Err(e) = decode::stream_url_to_output(&thread_url, &*sink, &thread_stop) {
                    eprintln!("playback ended: {e}");
                }
            })
            .map_err(|e| format!("failed to spawn decode thread: {e}"))?;

        zone_state.current = Some(Pipeline { stop, handle });
        Ok(())
    }

    /// Stop `zone`'s current playback. No-op if the zone is unknown or idle.
    pub fn stop(&self, zone: &str) {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        if let Some(zone_state) = zones.get_mut(zone) {
            if let Some(pipeline) = zone_state.current.take() {
                pipeline.shutdown();
            }
        }
    }

    /// Spawn the supervised `snapserver` if it isn't already running. Idempotent
    /// and lazy, mirroring [`ensure_local`](Self::ensure_local): the one place the
    /// snapserver process is launched, so a missing `snapserver` binary surfaces
    /// here (to the registering dongle) rather than at construction. Dongles can't
    /// hear anything until snapserver is up, so registration depends on this.
    fn ensure_snapcast(&self) -> Result<(), String> {
        let mut guard = self
            .snapserver
            .lock()
            .expect("engine snapserver mutex poisoned");
        if guard.is_none() {
            *guard = Some(SnapserverSupervisor::spawn(1)?);
        }
        Ok(())
    }

    /// Snapshot of the zones a client can target for playback, as
    /// `(zone, display_name, online)`, hub first. The hub's local output is
    /// reported as the synthesized `"default"` zone named `"Hub"` and is always
    /// listed (even before `ensure_local` lazily registers `"local"` on first
    /// play). Each dongle follows, by display name — its zone equals its output
    /// id, so no mapping is needed. Drives the iOS speaker picker.
    pub fn list_targets(&self) -> Vec<(ZoneId, String, bool)> {
        let mut dongles: Vec<(ZoneId, String, bool)> = self
            .registry
            .list()
            .into_iter()
            .filter(|(id, _, _)| id != LOCAL_OUTPUT_ID)
            .collect();
        dongles.sort_by(|a, b| a.1.cmp(&b.1));

        let mut targets = Vec::with_capacity(dongles.len() + 1);
        targets.push((
            DEFAULT_ZONE.to_string(),
            HUB_DISPLAY_NAME.to_string(),
            true,
        ));
        targets.extend(dongles);
        targets
    }

    /// Notify subscribers (per-client connections) that the output set changed so
    /// they re-push the target list. Fire-and-forget: a send error just means no
    /// client is currently listening.
    fn notify_outputs_changed(&self) {
        let _ = OUTPUTS_CHANGED.send(());
    }

    /// Register a dongle as an output and ensure `snapserver` is running so its
    /// `snapclient` has a stream to join. Called by the dongle registration
    /// listener (`server::dongle_server`) when a dongle connects. Re-registration
    /// (a dongle reconnecting) brings the existing output back online and keeps
    /// its zone — including any in-flight playback — intact. Errors only if
    /// `snapserver` can't be launched.
    pub fn register_dongle(&self, id: &str, name: &str) -> Result<(), String> {
        self.ensure_snapcast()?;
        self.add_dongle_output(id, name);
        self.notify_outputs_changed();
        Ok(())
    }

    /// Registry + zone bookkeeping for a dongle (no I/O — split out so it is
    /// unit-testable without a `snapserver`). The dongle's output points at the
    /// shared [`snapcast_sink`](Self::snapcast_sink); an auto-zone named after the
    /// dongle is created so `play {zone:<dongle>}` works before zone CRUD exists.
    fn add_dongle_output(&self, id: &str, name: &str) {
        self.registry.register(Output {
            id: id.to_string(),
            name: name.to_string(),
            sink: Arc::clone(&self.snapcast_sink),
            online: true,
        });
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones.entry(id.to_string()).or_insert_with(|| ZonePlayback {
            outputs: vec![id.to_string()],
            current: None,
        });
    }

    /// Mark a dongle's output unreachable when it disconnects. The output stays
    /// in the registry (so its zone/name persist for reconnection); it's just
    /// skipped when resolving sinks for playback. No-op if the id is unknown.
    pub fn dongle_offline(&self, id: &str) {
        self.registry.set_online(id, false);
        self.notify_outputs_changed();
    }

    /// Re-run Snapcast reconcile (fired by snapserver client-connect events).
    /// Bridge stub wired to the router in sub-step 3.4 Task 8; intentionally a
    /// no-op until the engine owns the SnapcastRouter.
    pub fn snapcast_on_notify(&self) {}
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// An [`AudioSink`] that forwards every write to several member sinks — the
/// basis for independent multi-room within a zone (Phase 2). It reports a fixed
/// canonical format that all members must accept; reconciling member device
/// rates is deferred to Change 5 (network sinks negotiate a shared format).
/// Only constructed for zones with ≥2 outputs, so it is unused until a real
/// second output exists.
struct FanOut {
    sinks: Vec<Arc<dyn AudioSink>>,
    sample_rate: u32,
    channels: u16,
}

impl FanOut {
    fn new(sinks: Vec<Arc<dyn AudioSink>>) -> Self {
        // Canonical CD-ish stereo format; member sinks are expected to accept
        // it. Proper negotiation is Change 5's job.
        Self {
            sinks,
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

impl AudioSink for FanOut {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn write(&self, samples: &[f32]) {
        for sink in &self.sinks {
            sink.write(samples);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Constructing the engine must not touch audio hardware.
    #[test]
    fn new_does_not_open_audio_device() {
        let engine = Engine::new();
        // The local output is not registered until the first play.
        assert!(engine.registry.sink(LOCAL_OUTPUT_ID).is_none());
    }

    // stop is a device-free no-op for both the default zone (idle) and an
    // unknown zone.
    #[test]
    fn stop_is_noop_without_playback() {
        let engine = Engine::new();
        engine.stop(DEFAULT_ZONE);
        engine.stop("nonexistent");
    }

    // play on an unknown zone errors before any device access.
    #[test]
    fn play_unknown_zone_errors() {
        let engine = Engine::new();
        let err = engine
            .play("nonexistent", "http://example.com/stream")
            .unwrap_err();
        assert_eq!(err, "unknown_zone");
    }

    // A dongle registers as an online output (resolving to the shared snapcast
    // sink) with an auto-zone named after it. Device-free: exercises only the
    // registry/zone bookkeeping, not the snapserver spawn.
    #[test]
    fn add_dongle_output_registers_and_creates_zone() {
        let engine = Engine::new();
        engine.add_dongle_output("dongle-1", "Kitchen");

        assert!(engine.registry.sink("dongle-1").is_some());
        let zones = engine.zones.lock().expect("zones");
        let zone = zones.get("dongle-1").expect("auto-zone created");
        assert_eq!(zone.outputs, vec!["dongle-1".to_string()]);
    }

    // list_targets always reports the synthesized "Hub" first (even with no
    // local output registered yet), then dongles by name, and includes offline
    // dongles with online=false. The "local" output id is never surfaced raw.
    #[test]
    fn list_targets_reports_hub_first_then_dongles() {
        let engine = Engine::new();
        engine.add_dongle_output("d-2", "Living Room");
        engine.add_dongle_output("d-1", "Bedroom");
        engine.dongle_offline("d-2");

        let targets = engine.list_targets();
        // Hub is always first and online, even though `local` isn't registered.
        assert_eq!(
            targets[0],
            (DEFAULT_ZONE.to_string(), HUB_DISPLAY_NAME.to_string(), true)
        );
        assert!(!targets.iter().any(|(zone, _, _)| zone == LOCAL_OUTPUT_ID));
        // Dongles follow, sorted by display name; the offline one is included.
        assert_eq!(
            targets[1],
            ("d-1".to_string(), "Bedroom".to_string(), true)
        );
        assert_eq!(
            targets[2],
            ("d-2".to_string(), "Living Room".to_string(), false)
        );
    }

    // Registering and marking a dongle offline each fire OUTPUTS_CHANGED so live
    // subscribers (per-client connections) re-push the target list.
    #[test]
    fn output_changes_notify_subscribers() {
        use tokio::sync::broadcast::error::TryRecvError;

        // A tick is observed if recv returns Ok or Lagged (other tests share this
        // global channel; Empty alone means our own send didn't land).
        fn ticked(rx: &mut broadcast::Receiver<()>) -> bool {
            !matches!(rx.try_recv(), Err(TryRecvError::Empty))
        }

        let engine = Engine::new();
        let mut rx = OUTPUTS_CHANGED.subscribe();

        // add_dongle_output is the device-free bookkeeping half (register_dongle
        // would also spawn snapserver), so notify explicitly to mirror it.
        engine.add_dongle_output("d-1", "Kitchen");
        engine.notify_outputs_changed();
        assert!(ticked(&mut rx));

        engine.dongle_offline("d-1");
        assert!(ticked(&mut rx));
    }

    // Disconnect marks the output offline (unresolvable for playback) but keeps
    // its zone so a reconnecting dongle keeps its identity.
    #[test]
    fn dongle_offline_unresolves_sink_but_keeps_zone() {
        let engine = Engine::new();
        engine.add_dongle_output("dongle-1", "Kitchen");
        engine.dongle_offline("dongle-1");

        assert!(engine.registry.sink("dongle-1").is_none());
        assert!(engine.zones.lock().expect("zones").contains_key("dongle-1"));
    }

    // Reconnecting (re-registering) an offline dongle brings it back online.
    #[test]
    fn re_register_brings_dongle_back_online() {
        let engine = Engine::new();
        engine.add_dongle_output("d", "Name");
        engine.dongle_offline("d");
        assert!(engine.registry.sink("d").is_none());

        engine.add_dongle_output("d", "Name");
        assert!(engine.registry.sink("d").is_some());
    }

    #[test]
    fn dongle_offline_unknown_is_noop() {
        let engine = Engine::new();
        engine.dongle_offline("nonexistent");
    }

    // Live end-to-end smoke test through the full engine: play the default zone
    // — which lazily opens the local device, registers it, resolves the
    // single-sink path, and streams — for ~3s, then stop. Requires network +
    // audio hardware, so it is opt-in:
    //   cargo test audio::engine::tests::engine_plays_default_zone_briefly -- --ignored --nocapture
    // You should hear audio.
    #[test]
    #[ignore]
    fn engine_plays_default_zone_briefly() {
        use std::thread;
        use std::time::Duration;

        const URL: &str = "https://ice1.somafm.com/groovesalad-128-mp3";

        let engine = Engine::new();
        // Local device is not opened until play.
        assert!(engine.registry.sink(LOCAL_OUTPUT_ID).is_none());

        engine.play(DEFAULT_ZONE, URL).expect("play should start");
        // ensure_local ran: the local output is now registered + online.
        assert!(engine.registry.sink(LOCAL_OUTPUT_ID).is_some());

        thread::sleep(Duration::from_secs(3));
        engine.stop(DEFAULT_ZONE);
    }

    // FanOut forwards writes to every member sink.
    #[test]
    fn fanout_forwards_to_all_sinks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counter(Arc<AtomicUsize>);
        impl AudioSink for Counter {
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn channels(&self) -> u16 {
                2
            }
            fn write(&self, samples: &[f32]) {
                self.0.fetch_add(samples.len(), Ordering::Relaxed);
            }
        }

        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let fanout = FanOut::new(vec![
            Arc::new(Counter(Arc::clone(&a))),
            Arc::new(Counter(Arc::clone(&b))),
        ]);

        fanout.write(&[0.0; 4]);
        assert_eq!(a.load(Ordering::Relaxed), 4);
        assert_eq!(b.load(Ordering::Relaxed), 4);
    }
}
