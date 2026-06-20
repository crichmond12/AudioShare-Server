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
//!    hold the connection open as a liveness signal. On disconnect, reconnect.
//!
//! Reconnect uses a fixed delay here; backoff/heartbeat hardening is sub-step 2.4.

mod assignment;
mod registration;
mod storage;
mod supervisor;

use std::time::Duration;

use storage::{HubAddress, Storage};

/// Delay between hub reconnection attempts. A simple fixed delay for sub-step 2.3
/// (backoff is 2.4).
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

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

    // Stay registered with the hub, reconnecting whenever the session drops.
    loop {
        if let Err(e) = registration::run_session(&identity, &hub).await {
            eprintln!("Hub session ended: {e}");
        }
        println!("Reconnecting to hub {hub} in {}s...", RECONNECT_DELAY.as_secs());
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
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
