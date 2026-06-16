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
/// The audio engine does not exist yet (KAN-18/20/21), so every recognized
/// command is acknowledged with a `not_yet_implemented` note rather than
/// actually producing audio. KAN-20 will replace these stubs with calls into
/// the real playback engine. `_data` is the command payload, unused for now.
pub fn dispatch(task: Task, _data: &Value) -> TaskResponse {
    match task {
        Task::Unknown(ref name) => {
            println!("Rejected unsupported task: {}", name);
            TaskResponse::error(task.name(), "unsupported_task")
        }
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

    #[test]
    fn known_task_dispatches_to_ok() {
        let json = dispatch(Task::Play, &Value::Null).to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"task\":\"play\""));
    }

    #[test]
    fn unknown_task_dispatches_to_error() {
        let json = dispatch(Task::parse("teleport"), &Value::Null).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unsupported_task\""));
        assert!(json.contains("\"task\":\"teleport\""));
    }
}
