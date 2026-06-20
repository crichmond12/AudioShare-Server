//! `snapclient` supervision (multi-room Change 5, sub-step 2.3).
//!
//! The dongle agent **delegates** audio transport + clock sync to Snapcast: it
//! runs a stock `snapclient` pointed at the hub's `snapserver` and keeps it
//! alive. It must never reimplement transport or sync itself — that discipline
//! is what keeps Snapcast a swappable implementation detail behind our agent
//! (see `docs/multi-room-plan.md`, Change 5).
//!
//! This mirrors the hub's `SnapserverSupervisor` (`src/audio/snapcast.rs`): a
//! thin supervisor that spawns the process, relaunches it if it exits, and kills
//! it on drop. The only differences are the binary (`snapclient`) and the args
//! (the hub's `snapserver` host/port + our dongle id as the Snapcast client id).

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long to wait before relaunching a `snapclient` that exited (e.g. the hub
/// went away). Matches the hub supervisor's restart cadence.
const SNAPCLIENT_RESTART_DELAY: Duration = Duration::from_secs(1);

/// Supervises one `snapclient` process pointed at the hub's `snapserver`.
///
/// Dropping it stops the supervisor and kills the live `snapclient`, so the agent
/// can tear playback down cleanly when its hub connection drops.
pub struct SnapclientSupervisor {
    stop: Arc<AtomicBool>,
    /// Shared so [`Drop`] can kill the live child immediately rather than wait
    /// out the monitor loop's next iteration.
    child: Arc<Mutex<Option<Child>>>,
    monitor: Option<JoinHandle<()>>,
}

impl SnapclientSupervisor {
    /// Spawn `snapclient` (resolved from `PATH`) connecting to `host:port` and
    /// announcing itself to Snapcast under `host_id` (the dongle's UUID, so the
    /// Snapcast client identity matches the hub's `OutputId` — forward-compatible
    /// with hub-driven grouping in sub-step 3).
    pub fn spawn(host: &str, port: u16, host_id: &str) -> Result<Self, String> {
        Self::spawn_with("snapclient", host, port, host_id)
    }

    /// Like [`spawn`](Self::spawn) but with an explicit binary (for tests/dev).
    /// Returns an error only if the *first* launch fails to spawn — later crashes
    /// are handled by the restart loop.
    pub fn spawn_with(
        binary: impl Into<String>,
        host: &str,
        port: u16,
        host_id: &str,
    ) -> Result<Self, String> {
        let binary = binary.into();
        let args = client_args(host, port, host_id);

        // Launch once up front so a misconfiguration (missing binary) surfaces to
        // the caller instead of being silently retried forever.
        let first = spawn_snapclient(&binary, &args)?;

        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(Some(first)));

        let monitor = {
            let stop = Arc::clone(&stop);
            let child = Arc::clone(&child);
            thread::Builder::new()
                .name("snapclient-supervisor".to_string())
                .spawn(move || monitor_loop(&binary, &args, &stop, &child))
                .map_err(|e| format!("failed to spawn snapclient supervisor thread: {e}"))?
        };

        Ok(Self {
            stop,
            child,
            monitor: Some(monitor),
        })
    }
}

impl Drop for SnapclientSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut child) = self
            .child
            .lock()
            .expect("snapclient child mutex poisoned")
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

/// Wait on the current child, and while not stopping, relaunch it after a short
/// delay if it exits.
fn monitor_loop(binary: &str, args: &[String], stop: &AtomicBool, child: &Mutex<Option<Child>>) {
    loop {
        let current = child
            .lock()
            .expect("snapclient child mutex poisoned")
            .take();
        if let Some(mut current) = current {
            let _ = current.wait();
        }

        if stop.load(Ordering::Relaxed) {
            return;
        }

        thread::sleep(SNAPCLIENT_RESTART_DELAY);
        if stop.load(Ordering::Relaxed) {
            return;
        }

        match spawn_snapclient(binary, args) {
            Ok(next) => *child.lock().expect("snapclient child mutex poisoned") = Some(next),
            Err(e) => {
                eprintln!("snapclient relaunch failed: {e}");
                return;
            }
        }
    }
}

/// Build the `snapclient` argument list for a hub `snapserver`.
fn client_args(host: &str, port: u16, host_id: &str) -> Vec<String> {
    vec![
        "-h".to_string(),
        host.to_string(),
        "-p".to_string(),
        port.to_string(),
        // Pin the Snapcast client id to our dongle UUID so the hub can address
        // this client by the same id it registered as an output.
        "--hostID".to_string(),
        host_id.to_string(),
    ]
}

/// Spawn one `snapclient` process.
fn spawn_snapclient(binary: &str, args: &[String]) -> Result<Child, String> {
    Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn snapclient `{binary}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_args_carry_host_port_and_id() {
        let args = client_args("192.168.1.10", 1704, "dongle-abc");
        assert_eq!(
            args,
            vec![
                "-h", "192.168.1.10", "-p", "1704", "--hostID", "dongle-abc"
            ]
        );
    }
}
