//! Hub registration (multi-room Change 5, sub-step 2.3).
//!
//! Once a dongle knows its assigned hub it connects to the hub's registration
//! listener ([`HUB_REGISTRATION_PORT`]), announces itself with
//! [`DongleToHub::Register`], and learns from [`HubToDongle::Registered`] where
//! its `snapclient` should point. The dongle then **holds that TCP connection
//! open** — the hub treats it as the dongle's liveness signal and marks the
//! output offline when it drops (see `src/server/dongle_server.rs`).
//!
//! [`run_session`] performs one full registration → supervise → hold cycle and
//! returns when the hub connection closes, so the caller can reconnect.
//!
//! While the session is held open the agent exchanges **heartbeats** with the hub
//! (sub-step 2.4): it sends a [`DongleToHub::Heartbeat`] every
//! [`HEARTBEAT_INTERVAL_SECS`] and times its reads out at
//! [`HEARTBEAT_TIMEOUT_SECS`], so a WiFi dropout where TCP never delivers a FIN
//! is detected within that window instead of hanging on a half-open socket.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::time::{interval, timeout};

use audioshare_protocol::{
    from_line, to_line, DongleToHub, HubToDongle, HEARTBEAT_INTERVAL_SECS, HEARTBEAT_TIMEOUT_SECS,
};

use crate::storage::{HubAddress, Identity};
use crate::supervisor::SnapclientSupervisor;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Aborts a spawned task when dropped, so the heartbeat sender never outlives the
/// session it belongs to (e.g. once the read loop returns on hub disconnect).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Connect to `hub`, register, start `snapclient` against the `snapserver` the hub
/// names, and hold the connection until the hub closes it (or an error occurs).
///
/// The [`SnapclientSupervisor`] is dropped as this returns, killing `snapclient`
/// so playback stops cleanly before the caller reconnects.
pub async fn run_session(identity: &Identity, hub: &HubAddress) -> Result<(), BoxError> {
    let stream = TcpStream::connect((hub.host.as_str(), hub.port)).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Announce ourselves.
    let register = DongleToHub::Register {
        dongle_id: identity.id.clone(),
        name: identity.name.clone(),
    };
    write_half
        .write_all(to_line(&register)?.as_bytes())
        .await?;

    // Learn where snapclient should connect (bounded so a silent hub can't stall
    // bring-up forever).
    let mut line = String::new();
    let read = timeout(
        Duration::from_secs(HEARTBEAT_TIMEOUT_SECS),
        reader.read_line(&mut line),
    )
    .await
    .map_err(|_| "hub did not reply to registration in time")??;
    if read == 0 {
        return Err("hub closed the connection before registering us".into());
    }
    let HubToDongle::Registered {
        snapserver_host,
        snapserver_port,
    } = from_line(&line)?
    else {
        return Err("expected Registered from hub".into());
    };
    println!(
        "Registered with hub {hub}; starting snapclient against {snapserver_host}:{snapserver_port}."
    );

    // Delegate audio + sync to snapclient; supervisor keeps it alive. Dropped on
    // return (hub disconnect), which kills snapclient and stops playback.
    let _snapclient =
        SnapclientSupervisor::spawn(&snapserver_host, snapserver_port, &identity.id)?;

    // Send heartbeats on a dedicated task so the timed read below is never
    // cancelled mid-line (read_line is not cancellation-safe). Aborted on return.
    let _heartbeat = AbortOnDrop(tokio::spawn(heartbeat_loop(write_half)));

    // Hold the connection open as our liveness signal, timing reads out so a dead
    // hub is detected even without a TCP FIN. Heartbeat replies (and any unknown
    // future messages) just refresh the deadline.
    loop {
        line.clear();
        match timeout(
            Duration::from_secs(HEARTBEAT_TIMEOUT_SECS),
            reader.read_line(&mut line),
        )
        .await
        {
            Err(_) => return Err("hub heartbeat timed out".into()),
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => continue, // heartbeat reply / forward-compat message
            Ok(Err(e)) => return Err(e.into()),
        }
    }

    println!("Hub {hub} closed the connection; stopping snapclient.");
    Ok(())
}

/// Send a [`DongleToHub::Heartbeat`] every [`HEARTBEAT_INTERVAL_SECS`] until the
/// write fails (the connection is gone) — at which point the read side will also
/// notice and end the session.
async fn heartbeat_loop(mut write_half: OwnedWriteHalf) {
    let mut ticker = interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    // First tick fires immediately; skip it so we don't double-send right after
    // registration.
    ticker.tick().await;
    let beat = match to_line(&DongleToHub::Heartbeat) {
        Ok(line) => line,
        Err(_) => return,
    };
    loop {
        ticker.tick().await;
        if write_half.write_all(beat.as_bytes()).await.is_err() {
            break;
        }
    }
}
