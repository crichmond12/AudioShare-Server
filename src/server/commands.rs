use crate::audio::engine::ENGINE;
use crate::json_structs::task_response::TaskResponse;
use serde_json::{json, Value};

/// A playback control command carried in the `task` field of an authenticated
/// runtime message. `Unknown` preserves the raw string so the dispatcher can
/// report exactly what was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum Task {
    Play,
    Pause,
    Stop,
    Seek,
    Volume,
    /// Client asks the hub to (re-)send the current speaker/zone list. Handled in
    /// `Connection::handle_task` (it needs the connection to push), not `dispatch`.
    ListOutputs,
    /// Client asks the hub to (re-)send the current AirPlay source list. Handled in
    /// `Connection::handle_task` (it needs the connection to push), not `dispatch`.
    ListSources,
    /// Client requests album art bytes for a given `art_id`. Returns inline data
    /// via `dispatch` (not a push), so no `handle_task` special-case is needed.
    GetArt,
    Reroute,
    CreateZone,
    DeleteZone,
    RenameZone,
    SetZoneOutputs,
    Unknown(String),
}

impl Task {
    pub fn parse(task: &str) -> Self {
        match task {
            "play" => Task::Play,
            "pause" => Task::Pause,
            "stop" => Task::Stop,
            "seek" => Task::Seek,
            "volume" => Task::Volume,
            "list_outputs" => Task::ListOutputs,
            "list_sources" => Task::ListSources,
            "get_art" => Task::GetArt,
            "reroute" => Task::Reroute,
            "create_zone" => Task::CreateZone,
            "delete_zone" => Task::DeleteZone,
            "rename_zone" => Task::RenameZone,
            "set_zone_outputs" => Task::SetZoneOutputs,
            other => Task::Unknown(other.to_string()),
        }
    }

    /// The canonical wire name, echoed back in the response's `task` field.
    fn name(&self) -> &str {
        match self {
            Task::Play => "play",
            Task::Pause => "pause",
            Task::Stop => "stop",
            Task::Seek => "seek",
            Task::Volume => "volume",
            Task::ListOutputs => "list_outputs",
            Task::ListSources => "list_sources",
            Task::GetArt => "get_art",
            Task::Reroute => "reroute",
            Task::CreateZone => "create_zone",
            Task::DeleteZone => "delete_zone",
            Task::RenameZone => "rename_zone",
            Task::SetZoneOutputs => "set_zone_outputs",
            Task::Unknown(name) => name,
        }
    }
}

/// Route a parsed task to its handler and produce the response to send back.
///
/// `play` and `stop` drive the real playback engine ([`ENGINE`]); the remaining
/// recognized tasks (`pause`/`seek`/`volume`) are still acknowledged with a
/// `not_yet_implemented` note. `data` is the command payload (e.g. `play`'s
/// stream URL and target `zone`).
pub fn dispatch(task: Task, data: &Value) -> TaskResponse {
    // The target zone defaults to "default" so clients that omit it (and the
    // single-zone setup) keep working. An empty string is treated as absent.
    let zone = match data["zone"].as_str() {
        Some(z) if !z.is_empty() => z,
        _ => "default",
    };
    match task {
        Task::Play => match data["url"].as_str() {
            Some(url) if !url.is_empty() => match ENGINE.play(zone, url) {
                Ok(()) => {
                    println!("Playing {} on zone {}", url, zone);
                    TaskResponse::accepted("play", None)
                }
                Err(e) => {
                    println!("Playback failed for {} on zone {}: {}", url, zone, e);
                    // Surface routing errors distinctly; everything else (device
                    // open, thread spawn) is a generic playback failure.
                    let code = match e.as_str() {
                        "unknown_zone" => "unknown_zone",
                        "zone_has_no_outputs" => "zone_has_no_outputs",
                        "no_free_stream" => "no_free_stream",
                        "mixed_zone_unsupported" => "mixed_zone_unsupported",
                        _ => "playback_failed",
                    };
                    TaskResponse::error("play", code)
                }
            },
            _ => {
                println!("Rejected play task: missing `data.url`");
                TaskResponse::error("play", "missing_url")
            }
        },
        Task::Stop => {
            // stop is a no-op for an unknown/idle zone, so it always succeeds.
            ENGINE.stop(zone);
            println!("Stopped playback on zone {}", zone);
            TaskResponse::accepted("stop", None)
        }
        Task::CreateZone => {
            let name = data["name"].as_str().unwrap_or("Zone");
            let id = ENGINE.create_zone(name);
            TaskResponse::accepted("create_zone", Some(json!({ "zone": id })))
        }
        Task::DeleteZone => match data["zone"].as_str() {
            Some(zone) if !zone.is_empty() => match ENGINE.delete_zone(zone) {
                Ok(()) => TaskResponse::accepted("delete_zone", None),
                Err(code) => TaskResponse::error("delete_zone", code),
            },
            _ => TaskResponse::error("delete_zone", "unknown_zone"),
        },
        Task::RenameZone => match (data["zone"].as_str(), data["name"].as_str()) {
            (Some(zone), Some(name)) if !zone.is_empty() => match ENGINE.rename_zone(zone, name) {
                Ok(()) => TaskResponse::accepted("rename_zone", None),
                Err(code) => TaskResponse::error("rename_zone", code),
            },
            _ => TaskResponse::error("rename_zone", "unknown_zone"),
        },
        Task::SetZoneOutputs => {
            let zone = data["zone"].as_str().unwrap_or("");
            let outputs: Vec<String> = data["outputs"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if zone.is_empty() {
                TaskResponse::error("set_zone_outputs", "unknown_zone")
            } else {
                match ENGINE.set_zone_outputs(zone, &outputs) {
                    Ok(()) => TaskResponse::accepted("set_zone_outputs", None),
                    Err(code) => TaskResponse::error("set_zone_outputs", code),
                }
            }
        }
        Task::GetArt => {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine as _;
            match data["art_id"].as_str() {
                Some(art_id) if !art_id.is_empty() => match ENGINE.get_art(art_id) {
                    Some((mime, bytes)) => TaskResponse::accepted(
                        "get_art",
                        Some(json!({
                            "art_id": art_id,
                            "mime": mime,
                            "image": STANDARD.encode(bytes),
                        })),
                    ),
                    None => TaskResponse::error("get_art", "unknown_art"),
                },
                _ => TaskResponse::error("get_art", "unknown_art"),
            }
        }
        Task::Reroute => {
            let source = data["source"].as_str().filter(|s| !s.is_empty());
            let zone = data["zone"].as_str().filter(|z| !z.is_empty());
            match (source, zone) {
                (None, _) => TaskResponse::error("reroute", "missing_source"),
                (Some(_), None) => TaskResponse::error("reroute", "missing_zone"),
                (Some(source), Some(zone)) => match ENGINE.reroute(source, zone) {
                    Ok(()) => {
                        println!("Rerouted source {} -> zone {}", source, zone);
                        TaskResponse::accepted("reroute", None)
                    }
                    Err(code) => {
                        println!("Reroute {} -> {} failed: {}", source, zone, code);
                        TaskResponse::error("reroute", code)
                    }
                },
            }
        }
        Task::Unknown(ref name) => {
            println!("Rejected unsupported task: {}", name);
            TaskResponse::error(task.name(), "unsupported_task")
        }
        // Pause/seek/volume are recognized but not implemented yet.
        ref known => {
            println!("Accepted task (stub): {}", known.name());
            TaskResponse::accepted(
                task.name(),
                Some(json!({ "note": "not_yet_implemented" })),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_structs::json_trait::JsonSerializable;

    #[test]
    fn parses_list_sources_task() {
        assert_eq!(Task::parse("list_sources"), Task::ListSources);
    }

    #[test]
    fn parses_known_and_unknown_tasks() {
        assert_eq!(Task::parse("play"), Task::Play);
        assert_eq!(Task::parse("volume"), Task::Volume);
        assert_eq!(
            Task::parse("teleport"),
            Task::Unknown("teleport".to_string())
        );
    }

    // Note: the `play` success path is exercised by the manual end-to-end test,
    // not here — it opens an audio device and hits the network, which a unit test
    // can't do deterministically. We test only the device-free routing below.

    #[test]
    fn stop_dispatches_to_ok() {
        // `stop` with nothing playing is a no-op and needs no audio device.
        let json = dispatch(Task::Stop, &Value::Null).to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"task\":\"stop\""));
    }

    #[test]
    fn play_without_url_errors_missing_url() {
        let json = dispatch(Task::Play, &Value::Null).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"missing_url\""));
        assert!(json.contains("\"task\":\"play\""));
    }

    #[test]
    fn recognized_but_unimplemented_task_is_ok_stub() {
        let json = dispatch(Task::Pause, &Value::Null).to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("not_yet_implemented"));
    }

    #[test]
    fn unknown_task_dispatches_to_error() {
        let json = dispatch(Task::parse("teleport"), &Value::Null).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unsupported_task\""));
        assert!(json.contains("\"task\":\"teleport\""));
    }

    #[test]
    fn play_unknown_zone_errors_before_touching_device() {
        // An unknown zone is rejected by the engine before any device access,
        // so this is device-free. The `default` zone is never targeted here.
        let data = serde_json::json!({ "url": "http://example.com/s", "zone": "nope" });
        let json = dispatch(Task::Play, &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_zone\""));
        assert!(json.contains("\"task\":\"play\""));
    }

    #[test]
    fn create_zone_returns_id() {
        let data = serde_json::json!({ "name": "Upstairs" });
        let json = dispatch(Task::parse("create_zone"), &data).to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"task\":\"create_zone\""));
        assert!(json.contains("\"zone\":\""));
    }

    #[test]
    fn delete_unknown_zone_errors() {
        let data = serde_json::json!({ "zone": "ghost" });
        let json = dispatch(Task::parse("delete_zone"), &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_zone\""));
    }

    #[test]
    fn set_zone_outputs_unknown_output_errors() {
        // Target the always-present default zone with a non-existent output.
        let data = serde_json::json!({ "zone": "default", "outputs": ["ghost"] });
        let json = dispatch(Task::parse("set_zone_outputs"), &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_output\""));
    }

    #[test]
    fn parses_get_art_task() {
        assert_eq!(Task::parse("get_art"), Task::GetArt);
        assert_eq!(Task::GetArt.name(), "get_art");
    }

    #[test]
    fn get_art_unknown_id_errors() {
        // No art cached -> any id is unknown_art. Device-free.
        let data = serde_json::json!({ "art_id": "deadbeef" });
        let json = dispatch(Task::GetArt, &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_art\""));
        assert!(json.contains("\"task\":\"get_art\""));
    }

    #[test]
    fn get_art_without_id_errors() {
        let json = dispatch(Task::GetArt, &Value::Null).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_art\""));
    }

    #[test]
    fn parses_reroute_task() {
        assert_eq!(Task::parse("reroute"), Task::Reroute);
    }

    #[test]
    fn reroute_without_source_errors_missing_source() {
        let json = dispatch(Task::Reroute, &json!({ "zone": "office" })).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"missing_source\""));
        assert!(json.contains("\"task\":\"reroute\""));
    }

    #[test]
    fn reroute_without_zone_errors_missing_zone() {
        let json = dispatch(Task::Reroute, &json!({ "source": "kitchen" })).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"missing_zone\""));
        assert!(json.contains("\"task\":\"reroute\""));
    }
}
