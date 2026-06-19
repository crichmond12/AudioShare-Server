//! Dongle registration listener (multi-room Change 5, sub-step 2.2).
//!
//! A TCP listener parallel to [`super::connection_server::ConnectServer`] (which
//! serves the iOS app on 50505). Dongles connect here on
//! [`HUB_REGISTRATION_PORT`] and announce themselves with a
//! [`DongleToHub::Register`]; the hub registers them as outputs and replies with
//! the `snapserver` coordinates their `snapclient` should join.
//!
//! The audio itself never crosses this socket — Snapcast carries it. This
//! channel is control only: register on connect, mark offline on disconnect.
//! It is intentionally **unauthenticated** for sub-step 2 (control-only, on the
//! user's own LAN, user-flashed dongles); see `docs/multi-room-plan.md`.

use std::sync::Arc;

use local_ip_address::local_ip;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use audioshare_protocol::{
    from_line, to_line, DongleToHub, HubToDongle, DEFAULT_SNAPSERVER_PORT, HUB_REGISTRATION_PORT,
};

use crate::audio::engine::ENGINE;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Listens for dongle registrations and routes them into the [`ENGINE`].
pub struct DongleServer {
    port: u16,
    /// The host a dongle's `snapclient` should connect to — i.e. this hub's LAN
    /// address, where its `snapserver` listens.
    snapserver_host: String,
}

impl DongleServer {
    pub fn new() -> Self {
        let snapserver_host = local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        Self {
            port: HUB_REGISTRATION_PORT,
            snapserver_host,
        }
    }

    pub async fn start_server(self: Arc<Self>) {
        let listener = match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to bind dongle registration listener: {e}");
                return;
            }
        };
        println!("Dongle registration listening on port {}", self.port);

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let server = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = server.handle(stream).await {
                            eprintln!("Dongle connection error ({addr}): {e}");
                        }
                    });
                }
                Err(e) => eprintln!("Dongle accept failed: {e}"),
            }
        }
    }

    /// Handle one dongle connection: read its `Register`, register it as an
    /// output, reply with the `snapserver` coordinates, then hold the connection
    /// open and mark the dongle offline when it drops.
    async fn handle(&self, stream: TcpStream) -> Result<(), BoxError> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // The first line must be a Register; a closed/empty connection is benign.
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let DongleToHub::Register { dongle_id, name } = from_line(&line)?;

        ENGINE.register_dongle(&dongle_id, &name)?;
        println!("Dongle registered: {dongle_id} ({name})");

        let reply = HubToDongle::Registered {
            snapserver_host: self.snapserver_host.clone(),
            snapserver_port: DEFAULT_SNAPSERVER_PORT,
        };
        write_half.write_all(to_line(&reply)?.as_bytes()).await?;

        // Keep the connection open: it is the dongle's liveness signal. Read until
        // EOF/error (no further message types are defined yet), then mark offline.
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        ENGINE.dongle_offline(&dongle_id);
        println!("Dongle offline: {dongle_id}");
        Ok(())
    }
}

impl Default for DongleServer {
    fn default() -> Self {
        Self::new()
    }
}
