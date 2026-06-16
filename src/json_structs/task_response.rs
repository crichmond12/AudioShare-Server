use serde::Serialize;
use serde_json::Value;
use super::json_trait::JsonSerializable;

/// Reply sent back over the encrypted channel after a `task` message is
/// dispatched. `status` is "ok" for an accepted command and "error" for a
/// rejected/failed one. `data` carries any command-specific payload; `error`
/// carries a machine-readable reason when `status` is "error".
#[derive(Serialize)]
pub struct TaskResponse {
    status: String,
    task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl TaskResponse {
    /// A command the server understood and accepted.
    pub fn accepted<T: Into<String>>(task: T, data: Option<Value>) -> Self {
        Self {
            status: "ok".to_string(),
            task: task.into(),
            data,
            error: None,
        }
    }

    /// A command the server rejected or could not complete. `error` is a stable
    /// machine-readable code (e.g. "unsupported_task").
    pub fn error<T: Into<String>, E: Into<String>>(task: T, error: E) -> Self {
        Self {
            status: "error".to_string(),
            task: task.into(),
            data: None,
            error: Some(error.into()),
        }
    }
}

impl JsonSerializable for TaskResponse {
    fn to_json(&self) -> String {
        match serde_json::to_string(self) {
            Ok(json_str) => json_str,
            Err(_) => "".to_string(),
        }
    }
}
