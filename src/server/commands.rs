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
            Task::Unknown(name) => name,
        }
    }
}

/// Route a parsed task to its handler and produce the response to send back.
///
/// `play` and `stop` drive the real playback engine ([`ENGINE`]); the remaining
/// recognized tasks (`pause`/`seek`/`volume`) are still acknowledged with a
/// `not_yet_implemented` note. `data` is the command payload (e.g. `play`'s
/// stream URL).
pub fn dispatch(task: Task, data: &Value) -> TaskResponse {
    match task {
        Task::Play => match data["url"].as_str() {
            Some(url) if !url.is_empty() => match ENGINE.play("default", url) {
                Ok(()) => {
                    println!("Playing: {}", url);
                    TaskResponse::accepted("play", None)
                }
                Err(e) => {
                    println!("Playback failed for {}: {}", url, e);
                    TaskResponse::error("play", "playback_failed")
                }
            },
            _ => {
                println!("Rejected play task: missing `data.url`");
                TaskResponse::error("play", "missing_url")
            }
        },
        Task::Stop => {
            ENGINE.stop("default");
            println!("Stopped playback");
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
}
