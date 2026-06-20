//! Dongle registration listener (multi-room Change 5, sub-step 2.2; heartbeat in 2.4).
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
//!
//! After registering, the dongle sends a [`DongleToHub::Heartbeat`] every
//! [`HEARTBEAT_INTERVAL_SECS`] and the hub replies in kind (sub-step 2.4). The
//! hub times its reads out at [`HEARTBEAT_TIMEOUT_SECS`], so a dongle that
//! vanishes without a TCP FIN (WiFi dropout) is still marked offline within that
//! window instead of holding a half-open connection forever.
//!
//! [`Engine`](crate::audio::engine::Engine) is reached through the
//! [`DongleRegistrar`] trait rather than the global `ENGINE` directly, so the
//! connection handling can be exercised with a mock in device-free tests.

use std::sync::Arc;
use std::time::Duration;

use local_ip_address::local_ip;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use audioshare_protocol::{
    from_line, to_line, DongleToHub, HubToDongle, DEFAULT_SNAPSERVER_PORT, HEARTBEAT_TIMEOUT_SECS,
    HUB_REGISTRATION_PORT,
};

use crate::audio::engine::ENGINE;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The engine actions the dongle listener needs. Abstracted from the global
/// [`ENGINE`] so [`DongleServer::handle`] can be tested with a mock that records
/// calls without spawning `snapserver` or touching audio hardware.
pub trait DongleRegistrar: Send + Sync {
    fn register_dongle(&self, id: &str, name: &str) -> Result<(), String>;
    fn dongle_offline(&self, id: &str);
}

/// Production registrar: forwards to the process-wide [`ENGINE`].
struct EngineRegistrar;

impl DongleRegistrar for EngineRegistrar {
    fn register_dongle(&self, id: &str, name: &str) -> Result<(), String> {
        ENGINE.register_dongle(id, name)
    }
    fn dongle_offline(&self, id: &str) {
        ENGINE.dongle_offline(id);
    }
}

/// Listens for dongle registrations and routes them into the engine.
pub struct DongleServer {
    port: u16,
    /// The host a dongle's `snapclient` should connect to — i.e. this hub's LAN
    /// address, where its `snapserver` listens.
    snapserver_host: String,
    registrar: Arc<dyn DongleRegistrar>,
}

impl DongleServer {
    pub fn new() -> Self {
        let snapserver_host = local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        Self {
            port: HUB_REGISTRATION_PORT,
            snapserver_host,
            registrar: Arc::new(EngineRegistrar),
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
    /// open — replying to heartbeats — and mark the dongle offline when it drops
    /// or stops heartbeating.
    async fn handle(&self, stream: TcpStream) -> Result<(), BoxError> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // The first line must be a Register; a closed/empty connection is benign.
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let DongleToHub::Register { dongle_id, name } = from_line(&line)? else {
            return Err("first message from dongle was not Register".into());
        };

        self.registrar.register_dongle(&dongle_id, &name)?;
        println!("Dongle registered: {dongle_id} ({name})");

        let reply = HubToDongle::Registered {
            snapserver_host: self.snapserver_host.clone(),
            snapserver_port: DEFAULT_SNAPSERVER_PORT,
        };
        write_half.write_all(to_line(&reply)?.as_bytes()).await?;

        // Keep the connection open as the dongle's liveness signal. Heartbeats
        // refresh the read deadline; a heartbeat reply lets the dongle detect a
        // dead hub too. EOF, an error, or no heartbeat within the timeout all end
        // the session and mark the output offline.
        loop {
            line.clear();
            match timeout(
                Duration::from_secs(HEARTBEAT_TIMEOUT_SECS),
                reader.read_line(&mut line),
            )
            .await
            {
                Err(_) => break, // no heartbeat in time: dongle is gone
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {
                    // Reply to heartbeats; ignore anything else (forward-compat).
                    if let Ok(DongleToHub::Heartbeat) = from_line::<DongleToHub>(&line) {
                        if write_half
                            .write_all(to_line(&HubToDongle::Heartbeat)?.as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Ok(Err(_)) => break,
            }
        }

        self.registrar.dongle_offline(&dongle_id);
        println!("Dongle offline: {dongle_id}");
        Ok(())
    }
}

impl Default for DongleServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    /// Records the engine calls the handler makes, without spawning `snapserver`.
    #[derive(Default)]
    struct MockRegistrar {
        registered: Mutex<Vec<(String, String)>>,
        offline: Mutex<Vec<String>>,
    }

    impl DongleRegistrar for MockRegistrar {
        fn register_dongle(&self, id: &str, name: &str) -> Result<(), String> {
            self.registered
                .lock()
                .unwrap()
                .push((id.to_string(), name.to_string()));
            Ok(())
        }
        fn dongle_offline(&self, id: &str) {
            self.offline.lock().unwrap().push(id.to_string());
        }
    }

    /// A full register → heartbeat → disconnect cycle over loopback, asserting the
    /// wire replies and that the registrar saw register-then-offline. Device-free:
    /// no `snapserver`, no audio hardware.
    #[tokio::test]
    async fn registers_replies_to_heartbeat_then_marks_offline() {
        let registrar = Arc::new(MockRegistrar::default());
        let server = Arc::new(DongleServer {
            port: 0,
            snapserver_host: "10.0.0.1".to_string(),
            registrar: Arc::clone(&registrar) as Arc<dyn DongleRegistrar>,
        });

        // Bind an ephemeral port and serve exactly one connection via `handle`.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                server.handle(stream).await.unwrap();
            })
        };

        // Client side: register, read the reply, heartbeat, read the reply, hang up.
        let client = TcpStream::connect(addr).await.unwrap();
        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);

        let register = DongleToHub::Register {
            dongle_id: "dongle-1".to_string(),
            name: "Kitchen".to_string(),
        };
        write_half
            .write_all(to_line(&register).unwrap().as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let registered: HubToDongle = from_line(&line).unwrap();
        assert_eq!(
            registered,
            HubToDongle::Registered {
                snapserver_host: "10.0.0.1".to_string(),
                snapserver_port: DEFAULT_SNAPSERVER_PORT,
            }
        );

        write_half
            .write_all(to_line(&DongleToHub::Heartbeat).unwrap().as_bytes())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(from_line::<HubToDongle>(&line).unwrap(), HubToDongle::Heartbeat);

        // Disconnect; the handler should mark the dongle offline and finish.
        drop(write_half);
        drop(reader);
        server_task.await.unwrap();

        assert_eq!(
            *registrar.registered.lock().unwrap(),
            vec![("dongle-1".to_string(), "Kitchen".to_string())]
        );
        assert_eq!(*registrar.offline.lock().unwrap(), vec!["dongle-1".to_string()]);
    }

    /// A connection that opens and closes without sending anything is benign and
    /// touches the registrar not at all.
    #[tokio::test]
    async fn empty_connection_is_ignored() {
        let registrar = Arc::new(MockRegistrar::default());
        let server = Arc::new(DongleServer {
            port: 0,
            snapserver_host: "10.0.0.1".to_string(),
            registrar: Arc::clone(&registrar) as Arc<dyn DongleRegistrar>,
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                server.handle(stream).await.unwrap();
            })
        };

        let client = TcpStream::connect(addr).await.unwrap();
        drop(client);
        server_task.await.unwrap();

        assert!(registrar.registered.lock().unwrap().is_empty());
        assert!(registrar.offline.lock().unwrap().is_empty());
    }
}
