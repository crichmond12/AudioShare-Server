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

// mDNS advertising splits by OS (see `advertise`): on Linux we publish through
// avahi (a subprocess), so the `mdns-sd` path and its helpers are only compiled
// for non-Linux (macOS dev).
#[cfg(not(target_os = "linux"))]
use std::collections::HashMap;

#[cfg(not(target_os = "linux"))]
use local_ip_address::local_ip;
#[cfg(not(target_os = "linux"))]
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
    // Keep the advert alive for the lifetime of this call; dropping it
    // unregisters the dongle, which is exactly what we want once assigned.
    let _advert = match advertise(identity) {
        Ok(advert) => Some(advert),
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

/// A live mDNS advert; dropping it withdraws the dongle from discovery.
///
/// The responder differs by OS. On **Linux** the system mDNS responder is
/// **avahi**, which owns port 5353 — running the `mdns-sd` crate as a *second*
/// responder on the same host registers the service internally but avahi
/// suppresses its records, so the advert never reaches the network (observed on
/// Raspberry Pi OS: the agent logged "Advertising" yet `avahi-browse` on the
/// same Pi saw nothing). So on Linux we publish *through* avahi via
/// `avahi-publish-service`, held as a child process for the advert's lifetime.
/// On **macOS** (dev) the system responder (mDNSResponder) coexists with
/// `mdns-sd` fine, so we keep the in-process path there. (The hub's
/// `broadcast.rs` has the same latent issue when run on a Pi.)
enum Advert {
    // Held only to keep the daemon (and thus the advert) alive until drop; the
    // field is never read, so suppress the dead-code lint on non-Linux builds.
    #[cfg(not(target_os = "linux"))]
    Mdns(#[allow(dead_code)] ServiceDaemon),
    #[cfg(target_os = "linux")]
    Avahi(std::process::Child),
}

#[cfg(target_os = "linux")]
impl Drop for Advert {
    fn drop(&mut self) {
        let Advert::Avahi(child) = self;
        // Stop the avahi registration (the advert is withdrawn when the client
        // process exits). Best-effort: a reaped child just means it already ended.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Advertise the dongle as `_audioshare-dongle._tcp` carrying its id and name in
/// TXT so the app can list it before assignment. See [`Advert`] for why the
/// implementation differs by OS.
fn advertise(identity: &Identity) -> Result<Advert, Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(target_os = "linux")]
    {
        advertise_avahi(identity)
    }
    #[cfg(not(target_os = "linux"))]
    {
        advertise_mdns(identity)
    }
}

/// Linux: publish through the system avahi daemon. Runs unprivileged (avahi is a
/// D-Bus client); requires `avahi-utils` (`avahi-publish-service`) installed.
#[cfg(target_os = "linux")]
fn advertise_avahi(identity: &Identity) -> Result<Advert, Box<dyn std::error::Error + Send + Sync>> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // avahi wants the bare service type (`_audioshare-dongle._tcp`), not the
    // `mdns-sd`-style `….local.` form the shared constant carries.
    let service_type = DONGLE_MDNS_SERVICE_TYPE.trim_end_matches(".local.");

    let mut command = Command::new("avahi-publish-service");
    command
        // Instance name the app sees in the browse list, then type, port, TXT.
        .arg(&identity.name)
        .arg(service_type)
        .arg(DONGLE_ASSIGNMENT_PORT.to_string())
        .arg(format!("id={}", identity.id))
        .arg(format!("name={}", identity.name))
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Backstop against orphaned adverts. The `Advert` `Drop` impl withdraws the
    // advert on a *graceful* shutdown, but the agent is normally stopped by a
    // signal (Ctrl-C / `kill` / crash / SIGKILL), which ends the process without
    // running destructors — leaving `avahi-publish-service` advertising forever,
    // so the app keeps listing a dongle that isn't running. Ask the kernel to
    // SIGTERM this child when its parent (the agent) dies by *any* means.
    //
    // SAFETY: the closure runs in the forked child before `exec` and calls only
    // the async-signal-safe `prctl`.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().map_err(|e| {
        format!("failed to spawn avahi-publish-service (is avahi-utils installed?): {e}")
    })?;

    Ok(Advert::Avahi(child))
}

/// Non-Linux (macOS dev): advertise in-process via `mdns-sd`.
#[cfg(not(target_os = "linux"))]
fn advertise_mdns(identity: &Identity) -> Result<Advert, Box<dyn std::error::Error + Send + Sync>> {
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
    Ok(Advert::Mdns(mdns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    fn identity() -> Identity {
        Identity {
            id: "dongle-1".to_string(),
            name: "Kitchen".to_string(),
        }
    }

    /// The app's assignment exchange over loopback: it sends `Assign` and the
    /// dongle replies `Assigned` and yields the chosen hub. Device-free (no mDNS,
    /// no snapclient) — exercises just the wire handshake.
    #[tokio::test]
    async fn assign_yields_hub_and_acks() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let dongle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_assignment(stream, &identity()).await.unwrap()
        });

        // Stand in for the app.
        let client = TcpStream::connect(addr).await.unwrap();
        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);

        let assign = AppToDongle::Assign {
            hub_host: "192.168.1.10".to_string(),
            hub_port: 50506,
        };
        write_half
            .write_all(to_line(&assign).unwrap().as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            from_line::<DongleToApp>(&line).unwrap(),
            DongleToApp::Assigned {
                dongle_id: "dongle-1".to_string()
            }
        );

        let hub = dongle.await.unwrap();
        assert_eq!(
            hub,
            Some(HubAddress {
                host: "192.168.1.10".to_string(),
                port: 50506,
            })
        );
    }

    /// A peer that connects then closes without sending an Assign is benign:
    /// `handle_assignment` returns `Ok(None)` so the listener keeps waiting.
    #[tokio::test]
    async fn empty_connection_yields_none() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let dongle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_assignment(stream, &identity()).await.unwrap()
        });

        let client = TcpStream::connect(addr).await.unwrap();
        drop(client);

        assert_eq!(dongle.await.unwrap(), None);
    }
}
