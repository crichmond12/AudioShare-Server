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

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use audioshare_protocol::{from_line, to_line, DongleToHub, HubToDongle};

use crate::storage::{HubAddress, Identity};
use crate::supervisor::SnapclientSupervisor;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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

    // Learn where snapclient should connect.
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err("hub closed the connection before registering us".into());
    }
    let HubToDongle::Registered {
        snapserver_host,
        snapserver_port,
    } = from_line(&line)?;
    println!(
        "Registered with hub {hub}; starting snapclient against {snapserver_host}:{snapserver_port}."
    );

    // Delegate audio + sync to snapclient; supervisor keeps it alive. Dropped on
    // return (hub disconnect), which kills snapclient and stops playback.
    let _snapclient =
        SnapclientSupervisor::spawn(&snapserver_host, snapserver_port, &identity.id)?;

    // Hold the connection open as our liveness signal. No further messages are
    // defined yet (heartbeat/grouping are later sub-steps); read until EOF.
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => continue, // ignore unknown future messages for forward-compat
            Err(e) => return Err(e.into()),
        }
    }

    println!("Hub {hub} closed the connection; stopping snapclient.");
    Ok(())
}
