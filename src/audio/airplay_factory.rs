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
use crate::audio::airplay_meta::{self, MetaAccumulator, MetaCommit};
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

        // Ensure the meta FIFO exists up front, so the only fallible step left
        // after the pump spawn is the meta-thread spawn itself (whose error path
        // tears the pump back down — a dropped JoinHandle detaches, never joins).
        let meta_fifo = airplay::meta_fifo_path(slot);
        airplay::ensure_fifo(&meta_fifo)?;

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

        // Metadata reader: pump shairport's metadata stream into the engine via
        // the accumulator. Runs continuously (independent of the audio session),
        // survives renames (FIFO persists).
        let meta = {
            let sessions = Arc::clone(&self.sessions);
            let source = zone.to_string();
            let stop = Arc::clone(&stop);
            let meta_fifo = meta_fifo.clone();
            thread::Builder::new()
                .name(format!("airplay-meta-{slot}"))
                .spawn(move || {
                    let mut acc = MetaAccumulator::new();
                    let result = airplay_meta::run_metadata_reader(&meta_fifo, &stop, |ev| {
                        if let Some(commit) = acc.apply(ev) {
                            match commit {
                                MetaCommit::Track { title, artist, album, client } => {
                                    sessions.track_update(&source, &title, &artist, &album, &client);
                                }
                                MetaCommit::Art(bytes) => sessions.art_update(&source, &bytes),
                            }
                        }
                    });
                    if let Err(e) = result {
                        eprintln!("airplay metadata reader for {source} ended: {e}");
                    }
                })
        };
        let meta = match meta {
            Ok(handle) => handle,
            Err(e) => {
                // The pump is already spawned; dropping its handle would detach it
                // and leak the thread parked on a blocking FIFO open. Tear it down
                // the same way Drop does: stop, nudge the audio FIFO open, join.
                stop.store(true, Ordering::Relaxed);
                let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
                let _ = pump.join();
                return Err(format!("failed to spawn airplay metadata thread: {e}"));
            }
        };

        Ok(Box::new(ShairportReceiver {
            slot,
            supervisor,
            stop,
            pump: Mutex::new(Some(pump)),
            meta: Mutex::new(Some(meta)),
            fifo,
            meta_fifo,
        }))
    }
}

/// One live receiver: the supervised process + its pump thread. Drop tears both
/// down (stop the pump, kill shairport via the supervisor's own Drop).
struct ShairportReceiver {
    slot: usize,
    supervisor: Mutex<Option<ShairportSupervisor>>,
    stop: Arc<AtomicBool>,
    pump: Mutex<Option<JoinHandle<()>>>,
    meta: Mutex<Option<JoinHandle<()>>>,
    fifo: PathBuf,
    meta_fifo: PathBuf,
}

impl ZoneReceiver for ShairportReceiver {
    fn rename(&self, new_name: &str) -> Result<(), String> {
        // Restart the supervisor with the new AirPlay/mDNS name; the pump thread
        // (and its FIFO) are unaffected.
        let next = ShairportSupervisor::spawn_for_slot(new_name, self.slot)?;
        *self.supervisor.lock().expect("supervisor mutex poisoned") = Some(next);
        Ok(())
    }
}

impl Drop for ShairportReceiver {
    fn drop(&mut self) {
        // Stop both readers first so neither blocks on a FIFO no writer will reopen.
        self.stop.store(true, Ordering::Relaxed);
        // Drop the supervisor (kills shairport), then nudge each blocking FIFO open
        // by briefly opening the write end so parked opens return and observe `stop`.
        *self.supervisor.lock().expect("supervisor mutex poisoned") = None;
        for fifo in [&self.fifo, &self.meta_fifo] {
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(fifo) {
                use std::io::Write;
                let _ = f.write_all(&[]);
            }
        }
        let pump = self.pump.lock().expect("pump mutex poisoned").take();
        if let Some(h) = pump {
            let _ = h.join();
        }
        let meta = self.meta.lock().expect("meta mutex poisoned").take();
        if let Some(h) = meta {
            let _ = h.join();
        }
    }
}

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
        fn track_update(&self, _s: &str, _title: &str, _artist: &str, _album: &str, _client: &str) {}
        fn art_update(&self, _s: &str, _image: &[u8]) {}
    }

    #[test]
    fn factory_constructs_without_spawning() {
        // Construction must not touch shairport-sync or any device.
        let _factory = ShairportReceiverFactory::new(Arc::new(NoSessions));
    }
}
