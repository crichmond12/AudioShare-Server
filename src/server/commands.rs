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
}
