//mod json_trait;
use super::json_trait::JsonSerializable;
use serde_json;
use serde::Serialize;

#[derive(Serialize)]
pub struct ServerResponseData {
    response: String
}

impl ServerResponseData {
    pub fn new<Message: Into<String>>(string: Message) -> Self {
        let response = string.into();
        Self {
            response: response,
        }
    }
}

impl JsonSerializable for ServerResponseData {
    fn to_json (&self) -> String {
        match serde_json::to_string(self) {
            Ok(json_str) => json_str,
            Err(_) => "".to_string(),
        }
    }
}

