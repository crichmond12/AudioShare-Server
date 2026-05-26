use crate::json_structs::json_trait::JsonSerializable;
use crate::json_structs::response_data::SessionKeyResponseData;
use crate::json_structs::server_response::ServerResponseData;
use crate::security::Security;
use crate::server::server::MAIN_SERVER;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

pub struct Connection<'connection> {
    security: Security,
    stream: &'connection mut TcpStream,
    client_uuid: Option<Uuid>,
}

impl<'connection> Connection<'connection> {
    pub fn new(security: Security, stream: &'connection mut TcpStream) -> Self {
        Self {
            security,
            stream,
            client_uuid: None,
        }
    }

    pub async fn start_new_connection(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let encrypted_session_key = self.security.get_encrypted_session_key()?;
        use base64::Engine as _;
        let encrypted_session_key_base64 = base64::engine::general_purpose::STANDARD.encode(&encrypted_session_key);
        let client_uuid = Uuid::new_v4();

        let server = MAIN_SERVER.lock().await;
        server
            .store_session(client_uuid, self.security.clone())
            .await;

        let response = SessionKeyResponseData::new(
            client_uuid.to_string(),
            encrypted_session_key_base64,
        );
        let server_response = ServerResponseData::new(response.to_json());

        self.stream
            .write_all(server_response.to_json().as_bytes())
            .await?;
        self.client_uuid = Some(client_uuid);

        println!("Started authenticated session: {}", client_uuid);
        Ok(())
    }

    pub async fn listen(&mut self) -> Result<bool, &'static str> {
        let mut buf = [0u8; 4096];
        loop {
            let read_result = timeout(Duration::from_secs(60), self.stream.read(&mut buf)).await;
            match read_result {
                Ok(Ok(n)) => {
                    if n == 0 {
                        return Ok(true);
                    }

                    let data = buf[..n].to_vec();
                    let encrypted_data = match String::from_utf8(data) {
                        Ok(encrypted_data) => encrypted_data,
                        Err(_) => {
                            println!("Data was not valid UTF-8.");
                            return Ok(true);
                        }
                    };

                    let decrypted_json = match self.security.decrypt_data(encrypted_data) {
                        Ok(decrypted_json) => decrypted_json,
                        Err(_) => {
                            println!("Error decrypting stream.");
                            return Ok(true);
                        }
                    };

                    let parsed_data = match serde_json::from_str::<serde_json::Value>(&decrypted_json) {
                        Ok(parsed_data) => parsed_data,
                        Err(_) => {
                            println!("Decrypted payload was not valid JSON.");
                            return Ok(true);
                        }
                    };

                    if !self.authenticate_message(&parsed_data).await {
                        let _ = self
                            .stream
                            .write_all(b"{\"error\":\"unauthorized\"}")
                            .await;
                        println!("Rejected unauthenticated request: {}", parsed_data);
                        return Ok(true);
                    }

                    println!("Authenticated request received: {}", parsed_data);
                }
                Ok(Err(_)) => return Ok(true),
                Err(_) => return Ok(true),
            }
        }
    }

    async fn authenticate_message(&self, request: &serde_json::Value) -> bool {
        let Some(expected_uuid) = self.client_uuid else {
            return false;
        };

        let Some(session_token) = request["session_token"].as_str() else {
            return false;
        };

        let Ok(request_uuid) = Uuid::parse_str(session_token) else {
            return false;
        };

        if request_uuid != expected_uuid {
            return false;
        }

        let server = MAIN_SERVER.lock().await;
        server.authenticate_session(&request_uuid).await
    }

    #[allow(dead_code)]
    pub async fn send_data(&mut self, data: &dyn JsonSerializable) -> bool {
        let response_str = data.to_json();
        let server_response = ServerResponseData::new(response_str);

        self.stream
            .write_all(server_response.to_json().as_bytes())
            .await
            .is_ok()
    }
}
