use serde::Serialize;
use super::json_trait::JsonSerializable;

#[derive(Serialize)]
pub struct SessionKeyResponseData {
    uuid: String,
    session: String,
}

impl SessionKeyResponseData {
    pub fn new(uuid: String, session_key: String) -> Self {
        Self {
            uuid: uuid,
            session: session_key,
        }
    }
}

impl JsonSerializable for SessionKeyResponseData {
    fn to_json (&self) -> String {
        match serde_json::to_string(self) {
            Ok(json_str) => json_str,
            Err(_) => "".to_string(),
        }
    }
}
