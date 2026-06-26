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

use crate::audio::airplay_manager::{ReceiverFactory, ShairportManager};
use crate::audio::decode;
use crate::audio::output::AudioOutput;
use crate::audio::registry::{Output, OutputId, OutputRegistry};
use crate::audio::sink::AudioSink;
use crate::audio::snapcast_router::SnapcastRouter;

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

    /// Broadcast tick fired whenever the set/state of active AirPlay sources changes
    /// (session begin/end, route/detach). Per-client connections re-push `sources`.
    pub static ref SOURCES_CHANGED: broadcast::Sender<()> = broadcast::channel(16).0;
}

/// The slice of the engine an AirPlay pump thread needs: bracket a session and
/// ask, per chunk, where the source's audio goes right now. A trait so the
/// production receiver factory can be wired without a hard dependency cycle and
/// the engine's session logic stays unit-testable in isolation.
pub trait SessionSink: Send + Sync {
    fn session_began(&self, source: &str);
    fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>>;
    fn session_ended(&self, source: &str);
    /// Slice 3: replace the source's track fields (empty `client` leaves it as-is).
    fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str);
    /// Slice 3: a new album-art image arrived for the source.
    fn art_update(&self, source: &str, image: &[u8]);
}

impl SessionSink for &'static Engine {
    fn session_began(&self, source: &str) { Engine::session_began(self, source); }
    fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>> {
        Engine::sink_for_source(self, source)
    }
    fn session_ended(&self, source: &str) { Engine::session_ended(self, source); }
    fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str) {
        Engine::track_update(self, source, title, artist, album, client);
    }
    fn art_update(&self, source: &str, image: &[u8]) { Engine::art_update(self, source, image); }
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
    // Slice 3: latest now-playing info; cleared on session end.
    title: String,
    artist: String,
    album: String,
    client: String,
    art_id: String, // sha256 hex of current art; "" when none
}

/// One cached album-art image (the latest for a source).
struct ArtImage {
    art_id: String,
    mime: String,
    bytes: Vec<u8>,
}

/// A source as reported to clients (active sessions only).
pub struct SourceView {
    pub source: ZoneId,
    pub name: String,
    pub dest_zone: ZoneId,
    pub routed: bool,
    // Slice 3:
    pub title: String,
    pub artist: String,
    pub album: String,
    pub client: String,
    pub art_id: String,
}

impl SourceView {
    #[cfg(test)]
    fn active_but_unrouted(&self) -> bool {
        !self.routed
    }
}

/// A zone's membership plus its current in-flight playback (if any).
struct ZonePlayback {
    name: String,
    outputs: Vec<OutputId>,
    current: Option<ZoneDriver>,
}

/// A zone as reported to clients: id, label, member output ids, and whether it
/// currently has playback.
pub struct ZoneView {
    pub zone: ZoneId,
    pub name: String,
    pub outputs: Vec<String>,
    pub playing: bool,
}

/// The multi-room playback engine. Shared process-wide via [`ENGINE`].
pub struct Engine {
    registry: Arc<OutputRegistry>,
    zones: Mutex<HashMap<ZoneId, ZonePlayback>>,
    /// The Snapcast router: owns the supervised snapserver, the stream pool,
    /// control connection, and event listener. Created lazily on first dongle
    /// zone play (inside `sink_for_zone`). Dongle outputs carry `sink: None`
    /// in the registry — the router allocates a per-zone FIFO sink at play time.
    snapcast: SnapcastRouter,
    /// Logical AirPlay sources keyed by source id (== home-zone id). Tracks
    /// session/routing state; an independent mutex from `zones`.
    sources: Mutex<HashMap<ZoneId, SourceState>>,
    /// Optional AirPlay receiver manager. `None` until `enable_airplay` is
    /// called. When present, reconciled against the zone set on every topology
    /// change (create/delete/rename zone, add dongle output). Held behind an
    /// `Arc` so `reconcile_airplay` can clone the handle and release the lock
    /// before calling `reconcile` (which may spawn a process in production).
    airplay: Mutex<Option<Arc<ShairportManager>>>,
    /// One latest album-art image per source id (Slice 3). Independent mutex;
    /// taken only after releasing `sources`.
    art_cache: Mutex<HashMap<ZoneId, ArtImage>>,
}

impl Engine {
    pub fn new() -> Self {
        // One default zone targeting the local device. Note: this does NOT open
        // the device — `ensure_local` does that lazily on first play.
        let mut zones = HashMap::new();
        zones.insert(
            DEFAULT_ZONE.to_string(),
            ZonePlayback {
                name: HUB_DISPLAY_NAME.to_string(),
                outputs: vec![LOCAL_OUTPUT_ID.to_string()],
                current: None,
            },
        );
        Self {
            registry: Arc::new(OutputRegistry::new()),
            zones: Mutex::new(zones),
            snapcast: SnapcastRouter::new(),
            sources: Mutex::new(HashMap::new()),
            airplay: Mutex::new(None),
            art_cache: Mutex::new(HashMap::new()),
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
            sink: Some(Arc::clone(&sink)),
            online: true,
        });
        Ok(sink)
    }

    /// Resolve a zone's online outputs to a single sink to decode into.
    /// Local zones open the cpal device lazily; dongle zones allocate a
    /// per-zone stream from the `SnapcastRouter`. Mixed zones are rejected.
    /// Errors if the zone has no reachable outputs or (dongle path) the pool
    /// is exhausted.
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

    /// Start streaming `url` to `zone`, replacing that zone's current playback.
    /// Other zones are unaffected. Returns an error if the zone is unknown, has
    /// no reachable outputs, or the local audio device can't be opened; later
    /// stream/decode failures surface on the decode thread and simply end
    /// playback.
    pub fn play(&self, zone: &str, url: &str) -> Result<(), String> {
        // Snapshot the zone's outputs under the lock, then RELEASE it: resolving
        // the sink can spawn snapserver / make snapserver JSON-RPC calls (a
        // network round-trip), and holding `zones` across that would stall every
        // other engine method that locks `zones`.
        let outputs = {
            let zones = self.zones.lock().expect("engine zones mutex poisoned");
            zones
                .get(zone)
                .map(|z| z.outputs.clone())
                .ok_or_else(|| "unknown_zone".to_string())?
        };

        let sink = self.zone_sink(zone, &outputs)?;

        // Reacquire only to swap in the new pipeline. The zone may have been
        // removed while the lock was released, and a concurrent play on the same
        // zone may have installed a pipeline — taking+shutting the existing one
        // before installing ours guarantees at most one active decoder per zone.
        // Take any existing driver under the lock, then release the lock before
        // detaching it: detach_driver may join a decode thread (URL) or lock
        // `sources` (Airplay), and must never run while holding `zones`.
        let prev = {
            let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
            // Ensure the zone still exists before we tear down anything.
            zones.get(zone).ok_or_else(|| "unknown_zone".to_string())?;
            zones.get_mut(zone).and_then(|z| z.current.take())
        };
        if let Some(prev) = prev {
            self.detach_driver(prev);
        }

        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let zone_state = zones.get_mut(zone).ok_or_else(|| "unknown_zone".to_string())?;

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

        zone_state.current = Some(ZoneDriver::Url(Pipeline { stop, handle }));
        Ok(())
    }

    /// Stop `zone`'s current playback. No-op if the zone is unknown or idle.
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

    /// Register a dongle as an output. Called by the dongle registration
    /// listener (`server::dongle_server`) when a dongle connects. Re-registration
    /// (a dongle reconnecting) brings the existing output back online and keeps
    /// its zone — including any in-flight playback — intact. Snapserver is NOT
    /// launched here; it starts lazily on the first dongle-zone `play` via
    /// `zone_sink` → `SnapcastRouter::sink_for_zone`. `reconcile_now` is called
    /// so a reconnecting dongle lands on the right stream if its zone is already
    /// playing. Always returns `Ok(())`.
    pub fn register_dongle(&self, id: &str, name: &str) -> Result<(), String> {
        self.add_dongle_output(id, name);
        self.notify_outputs_changed();
        // A reconnecting client may already be present; reconcile so it lands on
        // the right stream if its zone is playing.
        self.snapcast.reconcile_now();
        Ok(())
    }

    /// Registry + zone bookkeeping for a dongle (no I/O — split out so it is
    /// unit-testable without snapserver). The dongle output carries `sink: None`
    /// because it is grouped in snapserver; the router allocates a per-zone FIFO
    /// sink at play time. An auto-zone named after the dongle is created so
    /// `play {zone:<dongle>}` works before zone CRUD exists.
    fn add_dongle_output(&self, id: &str, name: &str) {
        self.registry.register(Output {
            id: id.to_string(),
            name: name.to_string(),
            sink: None,
            online: true,
        });
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        zones.entry(id.to_string()).or_insert_with(|| ZonePlayback {
            name: name.to_string(),
            outputs: vec![id.to_string()],
            current: None,
        });
        drop(zones); // release before reconcile_airplay, which re-locks zones
        self.reconcile_airplay();
    }

    /// Mark a dongle's output unreachable when it disconnects. The output stays
    /// in the registry (so its zone/name persist for reconnection); it's just
    /// skipped when resolving sinks for playback. No-op if the id is unknown.
    pub fn dongle_offline(&self, id: &str) {
        self.registry.set_online(id, false);
        self.notify_outputs_changed();
    }

    /// Create a new, empty, user-named zone and return its generated id.
    /// Duplicate names are allowed — the id is the identity.
    pub fn create_zone(&self, name: &str) -> ZoneId {
        let id = uuid::Uuid::new_v4().to_string();
        self.zones.lock().expect("engine zones mutex poisoned").insert(
            id.clone(),
            ZonePlayback { name: name.to_string(), outputs: Vec::new(), current: None },
        );
        self.notify_outputs_changed();
        self.reconcile_airplay();
        id
    }

    /// Delete a zone, stopping its playback and freeing its Snapcast stream.
    pub fn delete_zone(&self, zone: &str) -> Result<(), String> {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let removed = zones.remove(zone).ok_or_else(|| "unknown_zone".to_string())?;
        drop(zones);
        if let Some(driver) = removed.current {
            self.detach_driver(driver);
        }
        self.snapcast.release_zone(zone);
        self.notify_outputs_changed();
        self.reconcile_airplay();
        Ok(())
    }

    /// Rename a zone's label. Duplicate names are allowed.
    pub fn rename_zone(&self, zone: &str, name: &str) -> Result<(), String> {
        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        let z = zones.get_mut(zone).ok_or_else(|| "unknown_zone".to_string())?;
        z.name = name.to_string();
        drop(zones);
        self.notify_outputs_changed();
        self.reconcile_airplay();
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

    /// Re-run Snapcast reconcile (fired by snapserver client-connect events).
    pub fn snapcast_on_notify(&self) {
        self.snapcast.reconcile_now();
    }

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
            // Slice 3: now-playing is session-scoped.
            s.title.clear();
            s.artist.clear();
            s.album.clear();
            s.client.clear();
            s.art_id.clear();
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
        self.art_cache
            .lock()
            .expect("engine art cache mutex poisoned")
            .remove(source);

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
                title: s.title.clone(),
                artist: s.artist.clone(),
                album: s.album.clone(),
                client: s.client.clone(),
                art_id: s.art_id.clone(),
            })
            .collect()
    }

    /// Replace the source's track fields. An empty `client` leaves the existing
    /// value (shairport sends it sparsely). Fires SOURCES_CHANGED.
    pub fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str) {
        {
            let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
            let Some(s) = sources.get_mut(source) else { return };
            s.title = title.to_string();
            s.artist = artist.to_string();
            s.album = album.to_string();
            if !client.is_empty() {
                s.client = client.to_string();
            }
        }
        self.notify_sources_changed();
    }

    /// A new album-art image arrived for `source`: hash it (sha256 hex), sniff its
    /// mime, cache one image per source, and record its id on the source. Fires
    /// SOURCES_CHANGED.
    pub fn art_update(&self, source: &str, image: &[u8]) {
        let art_id = sha256::digest(image);
        let mime = sniff_mime(image).to_string();
        {
            let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
            let Some(s) = sources.get_mut(source) else { return };
            s.art_id = art_id.clone();
        }
        self.art_cache
            .lock()
            .expect("engine art cache mutex poisoned")
            .insert(source.to_string(), ArtImage { art_id, mime, bytes: image.to_vec() });
        self.notify_sources_changed();
    }

    /// Look up a cached image by its content hash. `None` => `unknown_art`.
    pub fn get_art(&self, art_id: &str) -> Option<(String, Vec<u8>)> {
        let cache = self.art_cache.lock().expect("engine art cache mutex poisoned");
        cache
            .values()
            .find(|a| a.art_id == art_id)
            .map(|a| (a.mime.clone(), a.bytes.clone()))
    }

    /// Notify subscribers that the active-source set/state changed.
    fn notify_sources_changed(&self) {
        let _ = SOURCES_CHANGED.send(());
    }

    /// Turn on AirPlay receiving: install the receiver manager and reconcile it
    /// (and the logical source registry) against the current zone set, spawning a
    /// receiver per zone. Idempotent-ish: a second call replaces the manager.
    pub fn enable_airplay(&self, factory: Box<dyn ReceiverFactory>) {
        *self.airplay.lock().expect("engine airplay mutex poisoned") =
            Some(Arc::new(ShairportManager::new(factory)));
        self.reconcile_airplay();
    }

    /// Converge the logical source registry and (if AirPlay is enabled) the
    /// receiver manager to the current zone set. Called after any zone-topology
    /// change.
    ///
    /// Lock discipline: snapshots `desired` under `zones` then releases it before
    /// locking `sources` or calling `mgr.reconcile` (which may spawn a process in
    /// production). Callers must NOT hold `zones` or `sources` when calling this.
    fn reconcile_airplay(&self) {
        // Desired = every zone (id, name). Snapshot under zones lock, then release.
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
                        title: String::new(),
                        artist: String::new(),
                        album: String::new(),
                        client: String::new(),
                        art_id: String::new(),
                    });
            }
        }

        // Snapshot the manager handle, release the airplay lock, THEN reconcile
        // outside the zones, sources, and airplay locks (reconcile may spawn a
        // shairport-sync process in production, so we must not hold any engine
        // lock across it). The guard is a temporary that drops at the `;`.
        let mgr = self.airplay.lock().expect("engine airplay mutex poisoned").clone();
        if let Some(mgr) = mgr {
            mgr.reconcile(&desired);
        }
    }
}

#[cfg(test)]
impl Engine {
    /// True if `id` has an entry in the sources map (regardless of active state).
    fn has_source(&self, id: &str) -> bool {
        self.sources.lock().unwrap().contains_key(id)
    }

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
                title: String::new(),
                artist: String::new(),
                album: String::new(),
                client: String::new(),
                art_id: String::new(),
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
        if let Some(prev) = prev {
            self.detach_driver(prev);
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort image mime from magic bytes; AirPlay art is JPEG or PNG.
fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else {
        "application/octet-stream"
    }
}

/// An [`AudioSink`] that forwards every write to several member sinks — the
/// basis for independent multi-room within a zone (Phase 2). It reports a fixed
/// canonical format that all members must accept; reconciling member device
/// rates is deferred to Change 5 (network sinks negotiate a shared format).
/// Only constructed for zones with ≥2 outputs, so it is unused until a real
/// second output exists. Reserved for multi-local-output zones.
#[allow(dead_code)]
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

    // A dongle registers as an online output (with no direct sink — grouped in
    // snapserver) with an auto-zone named after it. Device-free: exercises only
    // the registry/zone bookkeeping, not the snapserver spawn.
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

    fn dongle_online(engine: &Engine, id: &str) -> Option<bool> {
        engine.registry.list().into_iter().find(|(i, _, _)| i == id).map(|(_, _, on)| on)
    }

    // Disconnect marks the output offline (unresolvable for playback) but keeps
    // its zone so a reconnecting dongle keeps its identity.
    #[test]
    fn dongle_offline_unresolves_sink_but_keeps_zone() {
        let engine = Engine::new();
        engine.add_dongle_output("dongle-1", "Kitchen");
        engine.dongle_offline("dongle-1");

        assert_eq!(dongle_online(&engine, "dongle-1"), Some(false));
        assert!(engine.zones.lock().expect("zones").contains_key("dongle-1"));
    }

    // Reconnecting (re-registering) an offline dongle brings it back online.
    #[test]
    fn re_register_brings_dongle_back_online() {
        let engine = Engine::new();
        engine.add_dongle_output("d", "Name");
        engine.dongle_offline("d");
        assert_eq!(dongle_online(&engine, "d"), Some(false));

        engine.add_dongle_output("d", "Name");
        assert_eq!(dongle_online(&engine, "d"), Some(true));
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

    #[test]
    fn track_and_art_update_populate_source_view() {
        let engine = Engine::new();
        engine.add_idle_source("default", "Hub");
        engine.session_began("default"); // active so list_sources reports it

        engine.track_update("default", "Song", "Artist", "Album", "Chris's iPhone");
        engine.art_update("default", &[0xFF, 0xD8, 0xFF, 1, 2, 3]); // JPEG magic

        let views = engine.list_sources();
        let v = views.iter().find(|v| v.source == "default").expect("source present");
        assert_eq!(v.title, "Song");
        assert_eq!(v.artist, "Artist");
        assert_eq!(v.album, "Album");
        assert_eq!(v.client, "Chris's iPhone");
        assert!(!v.art_id.is_empty());

        // get_art returns the cached image + sniffed mime.
        let (mime, bytes) = engine.get_art(&v.art_id).expect("art cached");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, 1, 2, 3]);
    }

    #[test]
    fn get_art_unknown_id_returns_none() {
        let engine = Engine::new();
        assert!(engine.get_art("deadbeef").is_none());
    }

    #[test]
    fn session_ended_clears_track_and_art() {
        let engine = Engine::new();
        engine.add_idle_source("default", "Hub");
        engine.session_began("default");
        engine.track_update("default", "Song", "Artist", "Album", "Phone");
        engine.art_update("default", &[0x89, b'P', b'N', b'G', 9]);
        let art_id = engine.list_sources()[0].art_id.clone();

        engine.session_ended("default");

        // Source is now inactive (not in list_sources), and its art is evicted.
        assert!(engine.list_sources().is_empty());
        assert!(engine.get_art(&art_id).is_none());
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
