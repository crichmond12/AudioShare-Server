use crate::audio::engine::{ENGINE, OUTPUTS_CHANGED, SOURCES_CHANGED};
use crate::json_structs::json_trait::JsonSerializable;
use crate::json_structs::response_data::SessionKeyResponseData;
use crate::json_structs::server_response::ServerResponseData;
use crate::json_structs::task_response::TaskResponse;
use crate::security::Security;
use crate::server::commands;
use crate::server::server::MAIN_SERVER;
use serde_json::json;
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

        // Push the current speaker list once up front, then again on every
        // change, so the iOS picker is populated on connect and stays live.
        let mut outputs_changed = OUTPUTS_CHANGED.subscribe();
        if self.send_outputs().await.is_err() {
            return Ok(true);
        }
        if self.send_zones().await.is_err() {
            return Ok(true);
        }
        let mut sources_changed = SOURCES_CHANGED.subscribe();
        if self.send_sources().await.is_err() {
            return Ok(true);
        }

        loop {
            let read_result = tokio::select! {
                // Client → hub: an encrypted task message (or EOF/timeout).
                result = timeout(Duration::from_secs(60), self.stream.read(&mut buf)) => result,
                // Engine → client: outputs changed, re-push the snapshot. A
                // lagged receiver just means several changes coalesced; we always
                // send the full list, so re-pushing once catches up.
                _ = outputs_changed.recv() => {
                    if self.send_outputs().await.is_err() {
                        return Ok(true);
                    }
                    if self.send_zones().await.is_err() {
                        return Ok(true);
                    }
                    continue;
                }
                // Engine → client: AirPlay sources changed, re-push the snapshot.
                _ = sources_changed.recv() => {
                    if self.send_sources().await.is_err() {
                        return Ok(true);
                    }
                    continue;
                }
            };
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

                    if let Err(e) = self.handle_task(&parsed_data).await {
                        println!("Error responding to task: {}", e);
                        return Ok(true);
                    }
                }
                Ok(Err(_)) => return Ok(true),
                Err(_) => return Ok(true),
            }
        }
    }

    /// Parse the `task` field of an authenticated message, dispatch it, and
    /// send the (encrypted) response back. Unknown tasks get a structured error
    /// response but do not drop the connection — only a transport failure does.
    async fn handle_task(
        &mut self,
        request: &serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = match request["task"].as_str() {
            // `list_outputs` re-pushes the speaker list (it needs the connection,
            // so it's handled here rather than in the stateless `dispatch`).
            Some("list_outputs") => return self.send_outputs().await,
            Some("list_zones") => return self.send_zones().await,
            Some("list_sources") => return self.send_sources().await,
            Some(task_str) => {
                let task = commands::Task::parse(task_str);
                commands::dispatch(task, &request["data"])
            }
            None => {
                println!("Authenticated request missing `task` field: {}", request);
                TaskResponse::error("", "missing_task")
            }
        };

        self.send_encrypted(&response.to_json()).await
    }

    /// Push the current speaker/zone list to this client as an encrypted
    /// `{"status":"ok","task":"outputs","data":{"outputs":[...]}}` message. Sent
    /// on connect and whenever the output set changes, plus on a `list_outputs`
    /// request. Each entry is `{ "zone", "name", "online" }`.
    async fn send_outputs(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let outputs: Vec<serde_json::Value> = ENGINE
            .list_targets()
            .into_iter()
            .map(|(zone, name, online)| json!({ "zone": zone, "name": name, "online": online }))
            .collect();
        let response = TaskResponse::accepted("outputs", Some(json!({ "outputs": outputs })));
        self.send_encrypted(&response.to_json()).await
    }

    /// Push the current zone definitions (id, name, member outputs, playing) so a
    /// grouping UI can render membership. Additive to the flat `outputs` push.
    async fn send_zones(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let zones: Vec<serde_json::Value> = ENGINE
            .list_zones()
            .into_iter()
            .map(|z| json!({
                "zone": z.zone, "name": z.name, "outputs": z.outputs, "playing": z.playing
            }))
            .collect();
        let response = TaskResponse::accepted("zones", Some(json!({ "zones": zones })));
        self.send_encrypted(&response.to_json()).await
    }

    /// Push currently-active AirPlay sessions to this client as an encrypted
    /// `{"status":"ok","task":"sources","data":{"sources":[...]}}` message. Sent on
    /// connect, on every SOURCES_CHANGED tick, and on a `list_sources` request.
    async fn send_sources(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let sources: Vec<serde_json::Value> = ENGINE
            .list_sources()
            .into_iter()
            .map(|s| json!({
                "source": s.source, "name": s.name, "dest_zone": s.dest_zone,
                "active": true, "routed": s.routed,
                "title": s.title, "artist": s.artist, "album": s.album,
                "client": s.client, "art_id": s.art_id
            }))
            .collect();
        let response = TaskResponse::accepted("sources", Some(json!({ "sources": sources })));
        self.send_encrypted(&response.to_json()).await
    }

    /// Encrypt `json` with the session key and write it newline-framed. The `\n`
    /// delimits messages on the wire: task responses and unsolicited `outputs`
    /// pushes are multiplexed on one socket, and base64 (STANDARD) never contains
    /// `\n`, so the client can split cleanly. (The handshake response in
    /// `start_new_connection` is read separately and stays unframed.)
    async fn send_encrypted(
        &mut self,
        json: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut encrypted = self.security.encrypt_data(json.to_string())?.into_bytes();
        encrypted.push(b'\n');
        self.stream.write_all(&encrypted).await?;
        Ok(())
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
