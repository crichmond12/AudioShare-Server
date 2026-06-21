//! Snapcast control (JSON-RPC) client (multi-room Change 5, sub-step 3).
//!
//! A thin, synchronous client for `snapserver`'s control API on port 1705
//! (newline-delimited JSON-RPC 2.0). Only the oldest, most stable methods are
//! used — `Server.GetStatus`, `Group.SetClients`, `Group.SetStream` — so the hub
//! is insulated from snapserver version drift. Snapcast stays an implementation
//! detail behind [`SnapcastControl`]; the engine never sees this type.

use serde_json::Value;

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

#[cfg(test)]
mod tests {
    use super::*;

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
