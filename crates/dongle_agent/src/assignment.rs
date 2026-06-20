//! App-mediated discovery + assignment (multi-room Change 5, sub-step 2.3).
//!
//! An unassigned dongle does **not** auto-pick a hub (there may be several on one
//! LAN). Instead it advertises itself over mDNS as
//! [`DONGLE_MDNS_SERVICE_TYPE`] and runs a small assignment listener; the app —
//! already paired to one specific hub — discovers it, the user taps "add to this
//! hub," and the app sends [`AppToDongle::Assign`] with that hub's address. The
//! dongle persists it and switches to the registration flow. This puts the
//! "which hub" choice where the human + multi-hub knowledge live.
//!
//! [`await_assignment`] runs that flow and returns the chosen [`HubAddress`]; the
//! mDNS advert and listener are torn down as it returns (the dongle no longer
//! needs to be discoverable once assigned).

use std::collections::HashMap;

use local_ip_address::local_ip;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use audioshare_protocol::{
    from_line, to_line, AppToDongle, DongleToApp, DONGLE_ASSIGNMENT_PORT, DONGLE_MDNS_SERVICE_TYPE,
};

use crate::storage::{HubAddress, Identity};

/// Advertise this dongle and block until the app assigns it to a hub, returning
/// that hub's address. The caller persists it and proceeds to registration.
///
/// Per-connection errors (a malformed message, a client that hangs up early) are
/// logged and the listener keeps waiting — only a successful [`AppToDongle::Assign`]
/// ends the loop.
pub async fn await_assignment(identity: &Identity) -> std::io::Result<HubAddress> {
    // Keep the mDNS daemon alive for the lifetime of this call; dropping it
    // unregisters the advert, which is exactly what we want once assigned.
    let _mdns = match advertise(identity) {
        Ok(daemon) => Some(daemon),
        Err(e) => {
            // Discovery degrades to the `--hub` dev shortcut if mDNS is
            // unavailable, but the assignment listener can still take a direct
            // connection, so we carry on without it.
            eprintln!("mDNS advertise failed (dongle won't be auto-discoverable): {e}");
            None
        }
    };

    let listener = TcpListener::bind(("0.0.0.0", DONGLE_ASSIGNMENT_PORT)).await?;
    println!(
        "Unassigned. Advertising as \"{}\" ({}); waiting for a hub assignment on port {}.",
        identity.name, identity.id, DONGLE_ASSIGNMENT_PORT
    );

    loop {
        let (stream, addr) = listener.accept().await?;
        match handle_assignment(stream, identity).await {
            Ok(Some(hub)) => {
                println!("Assigned to hub {hub} by {addr}.");
                return Ok(hub);
            }
            Ok(None) => continue,
            Err(e) => {
                eprintln!("Assignment attempt from {addr} failed: {e}");
                continue;
            }
        }
    }
}

/// Handle one assignment connection: read an [`AppToDongle::Assign`], acknowledge
/// with [`DongleToApp::Assigned`], and return the hub address. `Ok(None)` means
/// the peer closed without sending anything (benign).
async fn handle_assignment(
    stream: TcpStream,
    identity: &Identity,
) -> Result<Option<HubAddress>, Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }

    let AppToDongle::Assign { hub_host, hub_port } = from_line(&line)?;

    let ack = DongleToApp::Assigned {
        dongle_id: identity.id.clone(),
    };
    write_half.write_all(to_line(&ack)?.as_bytes()).await?;

    Ok(Some(HubAddress {
        host: hub_host,
        port: hub_port,
    }))
}

/// Register the dongle's mDNS advert (`_audioshare-dongle._tcp`) carrying its id
/// and name in TXT so the app can list it before assignment. Mirrors the hub's
/// `broadcast.rs`.
fn advertise(identity: &Identity) -> Result<ServiceDaemon, Box<dyn std::error::Error + Send + Sync>> {
    let mdns = ServiceDaemon::new()?;
    let ip = local_ip()?.to_string();
    let host_name = format!("{ip}.local.");

    let mut properties = HashMap::new();
    properties.insert("id".to_string(), identity.id.clone());
    properties.insert("name".to_string(), identity.name.clone());

    let service = ServiceInfo::new(
        DONGLE_MDNS_SERVICE_TYPE,
        // Instance name the app sees in the browse list.
        &identity.name,
        &host_name,
        ip,
        DONGLE_ASSIGNMENT_PORT,
        properties,
    )?;

    mdns.register(service)?;
    Ok(mdns)
}
