//! Snapcast control (JSON-RPC) client (multi-room Change 5, sub-step 3).
//!
//! A thin, synchronous client for `snapserver`'s control API on port 1705
//! (newline-delimited JSON-RPC 2.0). Only the oldest, most stable methods are
//! used — `Server.GetStatus`, `Group.SetClients`, `Group.SetStream` — so the hub
//! is insulated from snapserver version drift. Snapcast stays an implementation
//! detail behind [`SnapcastControl`]; the engine never sees this type.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// One snapserver group: its id, the stream it plays, and its client ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupInfo {
    pub id: String,
    pub stream_id: String,
    pub clients: Vec<String>,
}

/// The slice of `Server.GetStatus` the reconciler needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerStatus {
    pub groups: Vec<GroupInfo>,
    connected: Vec<String>,
}

impl ServerStatus {
    /// The id of the group currently containing `client_id`, if any.
    pub fn group_of(&self, client_id: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|g| g.clients.iter().any(|c| c == client_id))
            .map(|g| g.id.as_str())
    }

    /// Whether a connected client with this id exists in any group.
    pub fn is_connected(&self, client_id: &str) -> bool {
        self.connected.iter().any(|c| c == client_id)
    }
}

/// Parse a `Server.GetStatus` JSON-RPC `result` into a [`ServerStatus`].
pub fn parse_server_status(result: &Value) -> Result<ServerStatus, String> {
    let groups_json = result["server"]["groups"]
        .as_array()
        .ok_or_else(|| "GetStatus: missing server.groups".to_string())?;

    let mut groups = Vec::with_capacity(groups_json.len());
    let mut connected = Vec::new();
    for g in groups_json {
        let id = g["id"].as_str().unwrap_or_default().to_string();
        let stream_id = g["stream_id"].as_str().unwrap_or_default().to_string();
        let mut clients = Vec::new();
        if let Some(cs) = g["clients"].as_array() {
            for c in cs {
                let cid = c["id"].as_str().unwrap_or_default().to_string();
                if c["connected"].as_bool().unwrap_or(false) {
                    connected.push(cid.clone());
                }
                clients.push(cid);
            }
        }
        groups.push(GroupInfo { id, stream_id, clients });
    }
    Ok(ServerStatus { groups, connected })
}

/// Hub → snapserver control surface, behind a trait so the reconciler can be
/// unit-tested against a mock with no real snapserver.
pub trait SnapcastControl: Send + Sync {
    fn get_status(&self) -> Result<ServerStatus, String>;
    fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String>;
    fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String>;
}

/// A synchronous JSON-RPC command connection to snapserver's control port.
///
/// One request → read lines until the matching `id` response (skipping any
/// interleaved notifications). A `Mutex` serializes callers so request/response
/// pairs never interleave on the socket.
pub struct CommandConn {
    inner: Mutex<ConnInner>,
    next_id: AtomicU64,
}

struct ConnInner {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl CommandConn {
    /// Open a control connection to `host:port` (snapserver's JSON-RPC port).
    pub fn connect(host: &str, port: u16) -> Result<Self, String> {
        let stream = TcpStream::connect((host, port))
            .map_err(|e| format!("snapserver control connect {host}:{port}: {e}"))?;
        let reader = BufReader::new(
            stream.try_clone().map_err(|e| format!("clone control socket: {e}"))?,
        );
        Ok(Self {
            inner: Mutex::new(ConnInner { writer: stream, reader }),
            next_id: AtomicU64::new(1),
        })
    }

    /// Issue one JSON-RPC call and return its `result` value.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let mut guard = self.inner.lock().expect("snapcast control mutex poisoned");
        let mut bytes = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        guard.writer.write_all(&bytes).map_err(|e| format!("control write: {e}"))?;

        // Read lines until the response whose id matches; skip notifications
        // (no `id`) that may interleave.
        loop {
            let mut line = String::new();
            let n = guard.reader.read_line(&mut line).map_err(|e| format!("control read: {e}"))?;
            if n == 0 {
                return Err("snapserver control closed the connection".to_string());
            }
            let msg: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg["id"].as_u64() == Some(id) {
                if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
                    return Err(format!("snapserver error: {err}"));
                }
                return Ok(msg["result"].clone());
            }
            // else: a notification or another id — keep reading.
        }
    }
}

impl SnapcastControl for CommandConn {
    fn get_status(&self) -> Result<ServerStatus, String> {
        let result = self.call("Server.GetStatus", json!({}))?;
        parse_server_status(&result)
    }

    fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String> {
        self.call("Group.SetClients", json!({ "id": group, "clients": clients }))?;
        Ok(())
    }

    fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String> {
        self.call("Group.SetStream", json!({ "id": group, "stream_id": stream }))?;
        Ok(())
    }
}

/// Listens on a dedicated snapserver control connection for client-(dis)connect
/// notifications and fires `on_event` so the router can reconcile. Uses a second
/// connection (not the command one) so reconcile-issued commands never deadlock
/// against this read loop.
pub struct EventListener {
    stop: Arc<AtomicBool>,
    stream: TcpStream,
    handle: Option<JoinHandle<()>>,
}

impl EventListener {
    pub fn spawn(
        host: &str,
        port: u16,
        on_event: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect((host, port))
            .map_err(|e| format!("snapserver event connect {host}:{port}: {e}"))?;
        let read_stream = stream.try_clone().map_err(|e| format!("clone event socket: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("snapcast-events".to_string())
                .spawn(move || {
                    let mut reader = BufReader::new(read_stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(msg) = serde_json::from_str::<Value>(line.trim()) {
                            if msg["method"].as_str().is_some_and(|m| {
                                m.starts_with("Client.") || m == "Server.OnUpdate"
                            }) {
                                on_event();
                            }
                        }
                    }
                })
                .map_err(|e| format!("spawn event thread: {e}"))?
        };

        Ok(Self { stop, stream, handle: Some(handle) })
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the read loop's blocking read_line.
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_conn_get_status_round_trips_over_tcp() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(req["method"], "Server.GetStatus");
            let id = req["id"].clone();
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "server": { "groups": [
                        { "id": "g0", "stream_id": "as-0",
                          "clients": [ { "id": "d1", "connected": true } ] }
                    ] }
                }
            });
            let mut stream = stream;
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            stream.write_all(&bytes).unwrap();
        });

        let conn = CommandConn::connect("127.0.0.1", addr.port()).expect("connect");
        let status = conn.get_status().expect("get_status");
        assert_eq!(status.group_of("d1"), Some("g0"));
        server.join().unwrap();
    }

    fn sample_status() -> serde_json::Value {
        serde_json::json!({
            "server": {
                "groups": [
                    {
                        "id": "group-A",
                        "stream_id": "as-0",
                        "clients": [
                            { "id": "dongle-1", "connected": true },
                            { "id": "dongle-2", "connected": false }
                        ]
                    },
                    {
                        "id": "group-B",
                        "stream_id": "as-1",
                        "clients": [ { "id": "dongle-3", "connected": true } ]
                    }
                ]
            }
        })
    }

    #[test]
    fn parses_groups_clients_and_streams() {
        let status = parse_server_status(&sample_status()).expect("parse");
        assert_eq!(status.groups.len(), 2);
        assert_eq!(status.group_of("dongle-1"), Some("group-A"));
        assert_eq!(status.group_of("dongle-3"), Some("group-B"));
        assert_eq!(status.group_of("nope"), None);
        assert!(status.is_connected("dongle-1"));
        assert!(!status.is_connected("dongle-2"));
        assert!(!status.is_connected("nope"));
    }
}
