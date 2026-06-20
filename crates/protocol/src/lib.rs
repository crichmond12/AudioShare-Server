//! Audio Share hub ↔ dongle control protocol (multi-room Change 5, sub-step 2).
//!
//! This crate is the **single source of truth** for the control/registration
//! wire format between the hub (`audio_share`) and the custom dongle agent
//! (`crates/dongle_agent`). It is deliberately dependency-light (serde only) and
//! carries **no audio** — Snapcast transports the audio; this channel only does
//! registration, assignment, and (later) supervision/grouping control. Sharing
//! these types across both binaries is the reason the dongle agent lives in this
//! workspace instead of a separate repo: the hub↔dongle contract can't drift the
//! way the iOS protocol does (see `CLAUDE.md`).
//!
//! ## Framing
//! Messages are **newline-delimited JSON** over TCP: one JSON object per line,
//! terminated by `\n`. Use [`to_line`] / [`from_line`] so framing stays
//! consistent on both ends (and stays debuggable with `nc`). Each direction is a
//! `#[serde(tag = "type")]` enum so new message kinds can be added without
//! breaking existing parsers.
//!
//! ## Directions (see `docs/multi-room-plan.md`, Change 5 sub-step 2)
//! - [`DongleToHub`] — the agent registers itself with its assigned hub.
//! - [`HubToDongle`] — the hub tells the agent where to point `snapclient`.
//! - [`AppToDongle`] — the app claims an unassigned dongle for a specific hub
//!   (app-mediated discovery: resolves multiple hubs on one LAN).
//! - [`DongleToApp`] — the agent acknowledges the assignment.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// TCP port the hub's dongle-registration listener binds (parallel to the iOS
/// `ConnectServer` on 50505). Dongles connect here to send [`DongleToHub`].
pub const HUB_REGISTRATION_PORT: u16 = 50506;

/// TCP port an *unassigned* dongle's assignment listener binds. The app connects
/// here to send [`AppToDongle::Assign`] once the user picks a hub for it.
pub const DONGLE_ASSIGNMENT_PORT: u16 = 50507;

/// mDNS service type an unassigned dongle advertises so the app can discover it.
/// (The hub advertises `_audioshare._tcp.local.`; this is the dongle's own.)
pub const DONGLE_MDNS_SERVICE_TYPE: &str = "_audioshare-dongle._tcp.local.";

/// Default Snapcast stream port a dongle's `snapclient` connects to on the hub.
pub const DEFAULT_SNAPSERVER_PORT: u16 = 1704;

/// How often the dongle sends a [`DongleToHub::Heartbeat`] while registered
/// (Change 5 sub-step 2.4). Both ends share this constant so the sender's
/// cadence and the receiver's [`HEARTBEAT_TIMEOUT_SECS`] can't drift apart.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 5;

/// How long either end waits for a heartbeat (or any line) before treating the
/// peer as gone. Set to several missed intervals so a single dropped packet or
/// a brief scheduling hiccup doesn't tear the session down; a real WiFi dropout
/// (where TCP never delivers a FIN) is then caught within this window instead of
/// hanging on a half-open connection forever.
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 15;

/// Agent → hub. Sent once the dongle knows its assigned hub (after [`AppToDongle::Assign`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DongleToHub {
    /// Announce this dongle as an available output. `dongle_id` is the dongle's
    /// persisted UUID (becomes the hub's `OutputId`); `name` is a human label
    /// (defaults to the hostname).
    Register { dongle_id: String, name: String },
    /// Liveness ping sent every [`HEARTBEAT_INTERVAL_SECS`] while registered. The
    /// hub replies with [`HubToDongle::Heartbeat`]; absence past
    /// [`HEARTBEAT_TIMEOUT_SECS`] marks the dongle offline (Change 5 sub-step 2.4).
    Heartbeat,
}

/// Hub → agent. The reply that tells the agent how to start `snapclient`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HubToDongle {
    /// Registration accepted; point `snapclient` at this `snapserver`.
    Registered {
        snapserver_host: String,
        snapserver_port: u16,
    },
    /// Reply to a [`DongleToHub::Heartbeat`]. Lets the agent detect a dead hub
    /// (its own read times out when these stop arriving) (Change 5 sub-step 2.4).
    Heartbeat,
}

/// App → unassigned dongle. The app (paired to one specific hub) claims the
/// dongle for that hub. The dongle persists the address and then registers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AppToDongle {
    /// Claim this dongle for the hub reachable at `hub_host:hub_port` (the hub's
    /// [`HUB_REGISTRATION_PORT`]).
    Assign { hub_host: String, hub_port: u16 },
}

/// Dongle → app. Acknowledges an [`AppToDongle::Assign`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DongleToApp {
    /// The dongle accepted the assignment and will register with the given hub.
    Assigned { dongle_id: String },
}

/// Serialize a message to a single newline-terminated JSON line for the wire.
pub fn to_line<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Parse one newline-delimited JSON line (trailing newline optional) into a message.
pub fn from_line<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_round_trips_through_a_line() {
        let msg = DongleToHub::Register {
            dongle_id: "abc-123".to_string(),
            name: "Kitchen".to_string(),
        };
        let line = to_line(&msg).expect("serialize");
        assert!(line.ends_with('\n'), "framing must terminate with a newline");
        let back: DongleToHub = from_line(&line).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn each_direction_round_trips() {
        let registered = HubToDongle::Registered {
            snapserver_host: "192.168.1.10".to_string(),
            snapserver_port: DEFAULT_SNAPSERVER_PORT,
        };
        assert_eq!(
            registered,
            from_line(&to_line(&registered).unwrap()).unwrap()
        );

        let assign = AppToDongle::Assign {
            hub_host: "192.168.1.10".to_string(),
            hub_port: HUB_REGISTRATION_PORT,
        };
        assert_eq!(assign, from_line(&to_line(&assign).unwrap()).unwrap());

        let assigned = DongleToApp::Assigned {
            dongle_id: "abc-123".to_string(),
        };
        assert_eq!(assigned, from_line(&to_line(&assigned).unwrap()).unwrap());

        // Heartbeats (sub-step 2.4) carry no payload but must still round-trip.
        let beat_up = DongleToHub::Heartbeat;
        assert_eq!(beat_up, from_line(&to_line(&beat_up).unwrap()).unwrap());
        let beat_down = HubToDongle::Heartbeat;
        assert_eq!(beat_down, from_line(&to_line(&beat_down).unwrap()).unwrap());
    }

    #[test]
    fn from_line_tolerates_missing_trailing_newline() {
        let raw = r#"{"type":"Register","dongle_id":"x","name":"y"}"#;
        let msg: DongleToHub = from_line(raw).expect("parse without newline");
        assert_eq!(
            msg,
            DongleToHub::Register {
                dongle_id: "x".to_string(),
                name: "y".to_string()
            }
        );
    }

    #[test]
    fn tagged_representation_is_stable() {
        // The wire shape other implementations (iOS app for Assign) must match.
        let line = to_line(&AppToDongle::Assign {
            hub_host: "h".to_string(),
            hub_port: 50506,
        })
        .unwrap();
        assert_eq!(line, "{\"type\":\"Assign\",\"hub_host\":\"h\",\"hub_port\":50506}\n");
    }
}
