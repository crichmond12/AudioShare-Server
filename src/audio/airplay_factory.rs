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
