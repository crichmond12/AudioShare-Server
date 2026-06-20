//! Audio Share **dongle agent** (multi-room Change 5, sub-step 2.3).
//!
//! Runs on a small networked receiver wired to a speaker. It owns the dongle's
//! identity + hub assignment + registration, and **supervises** a stock
//! `snapclient` — it never does audio transport or clock-sync itself (Snapcast
//! does, behind this seam). See `docs/multi-room-plan.md`, Change 5.
//!
//! Lifecycle:
//! 1. Load (or create) a persisted identity (UUID + name).
//! 2. Determine the hub: `--hub` flag (dev shortcut) > persisted assignment >
//!    otherwise advertise over mDNS and wait for the app to assign one.
//! 3. Register with the hub, start `snapclient` against its `snapserver`, and
//!    hold the connection open as a liveness signal. On disconnect, reconnect
//!    with exponential backoff (sub-step 2.4).

mod assignment;
mod registration;
mod storage;
mod supervisor;

use std::time::{Duration, Instant};

use storage::{HubAddress, Storage};

/// Reconnect backoff bounds (sub-step 2.4). Start fast so a quick hub restart is
/// barely noticed, then back off to a ceiling so a long-down hub isn't hammered.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A session that stayed up at least this long is treated as healthy, so the
/// backoff resets to [`BACKOFF_BASE`] for the next drop. Longer than the
/// heartbeat timeout so a session that died on a missed heartbeat (an unhealthy
/// one) does *not* reset the backoff.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);

struct Args {
    /// Dev-only bring-up shortcut: skip discovery and use this hub directly.
    hub: Option<HubAddress>,
    /// Override the persisted/default dongle name.
    name: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("usage: dongle_agent [--hub <host[:port]>] [--name <name>]");
            std::process::exit(2);
        }
    };

    let storage = Storage::new();
    let identity = match storage.load_or_create_identity(args.name) {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("Failed to load dongle identity: {e}");
            std::process::exit(1);
        }
    };
    println!("Dongle \"{}\" (id {})", identity.name, identity.id);

    // Resolve the hub: explicit flag wins (and is persisted so it sticks), else a
    // prior assignment, else run app-mediated discovery until assigned.
    let hub = match args.hub {
        Some(hub) => {
            if let Err(e) = storage.save_hub(&hub) {
                eprintln!("Warning: could not persist --hub assignment: {e}");
            }
            hub
        }
        None => match storage.load_hub() {
            Some(hub) => {
                println!("Using assigned hub {hub}.");
                hub
            }
            None => match assignment::await_assignment(&identity).await {
                Ok(hub) => {
                    if let Err(e) = storage.save_hub(&hub) {
                        eprintln!("Warning: could not persist hub assignment: {e}");
                    }
                    hub
                }
                Err(e) => {
                    eprintln!("Assignment listener failed: {e}");
                    std::process::exit(1);
                }
            },
        },
    };

    // Stay registered with the hub, reconnecting with exponential backoff
    // whenever the session drops. A session that stayed healthy resets the delay.
    let mut backoff = BACKOFF_BASE;
    loop {
        let started = Instant::now();
        if let Err(e) = registration::run_session(&identity, &hub).await {
            eprintln!("Hub session ended: {e}");
        }
        if started.elapsed() >= BACKOFF_RESET_AFTER {
            backoff = BACKOFF_BASE;
        }
        println!("Reconnecting to hub {hub} in {}s...", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

/// Double the reconnect delay, capped at [`BACKOFF_MAX`]. Pure so it's unit-tested
/// without sleeping.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
}

/// Minimal hand-rolled flag parsing (avoids a CLI dep for two options).
fn parse_args() -> Result<Args, String> {
    let mut hub = None;
    let mut name = None;
    let mut argv = std::env::args().skip(1);

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--hub" => {
                let value = argv.next().ok_or("--hub requires a value")?;
                hub = Some(value.parse::<HubAddress>()?);
            }
            "--name" => {
                name = Some(argv.next().ok_or("--name requires a value")?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args { hub, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        // Grows geometrically from the base and saturates at the ceiling.
        assert_eq!(next_backoff(BACKOFF_BASE), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(8)), Duration::from_secs(16));
        assert_eq!(next_backoff(BACKOFF_MAX), BACKOFF_MAX);
        assert_eq!(next_backoff(Duration::from_secs(20)), BACKOFF_MAX);
    }
}
