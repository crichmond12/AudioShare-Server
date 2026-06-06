use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use local_ip_address::local_ip;
use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;
use super::connection::Connection;
use crate::security::Security;
use crate::pairing;

enum ConnectionActions {
    StartConnection(serde_json::Value),
    Disconnect(bool),
}

pub struct ConnectServer {
    pub serial_number: String,
    pub ip: String,
    pub server_port: i32,
    pub pairing_secret: [u8; 32],
}

impl ConnectServer {
    pub fn new() -> Self {
        let my_ip_address = local_ip().unwrap().to_string();

        let pairing_secret = pairing::load_or_create(Path::new(pairing::PAIRING_SECRET_PATH))
            .expect("Failed to load or create pairing secret — cannot start securely");

        match Self::get_serial_number() {
            Ok(serial_number) => {
                let qr = pairing::qr_payload(&serial_number, &pairing_secret);
                println!("=== SCAN THIS QR CODE TO PAIR ===");
                println!("{}", qr);
                println!("=================================");
                Self {
                    serial_number,
                    ip: my_ip_address,
                    server_port: 50505,
                    pairing_secret,
                }
            }
            Err(e) => {
                eprintln!("Fatal: could not read serial number: {}", e);
                std::process::exit(1);
            }
        }
    }

    pub async fn start_server(self: Arc<Self>) {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", self.server_port))
            .await
            .expect("Failed to bind TCP listener");
        println!("Server listening on port {}", self.server_port);

        loop {
            match listener.accept().await {
                Ok((mut stream, _)) => {
                    let server = Arc::clone(&self);
                    println!("CONNECTING");
                    tokio::spawn(async move {
                        match server.get_connection_action(&mut stream).await {
                            Ok(ConnectionActions::StartConnection(json)) => {
                                match Security::get_public_key_from_request(json) {
                                    Ok(public_key) => {
                                        println!("Got public");
                                        let pairing_secret = server.pairing_secret; // [u8;32] is Copy
                                        let security = Security::new(public_key, pairing_secret)
                                            .expect("Error creating new security module.");
                                        let mut connection = Connection::new(security, &mut stream);
                                        println!("Start New CONNECTION!!!");
                                        if let Err(e) = connection.start_new_connection().await {
                                            println!("Error starting authenticated connection: {}", e);
                                            return;
                                        }
                                        println!("LISTEN");
                                        if let Err(e) = connection.listen().await {
                                            println!("Error listening on authenticated connection: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        println!("ERROR: {}", e);
                                        return;
                                    }
                                }
                            }
                            Ok(ConnectionActions::Disconnect(_disconnect)) => {
                                println!("DISCONNECT");
                            }
                            Err(e) => {
                                println!("{}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    println!("Connection failed: {}", e);
                }
            }
        }
    }

    async fn get_connection_action(
        &self,
        stream: &mut TcpStream,
    ) -> Result<ConnectionActions, Box<dyn std::error::Error + Send + Sync>> {
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf).await {
            Ok(n) if n == 0 => return Ok(ConnectionActions::Disconnect(true)),
            Ok(n) => {
                println!("Connection Received");
                let data = &buf[..n];
                match serde_json::from_slice(data) {
                    Ok(parsed_data) => {
                        println!("JSON RECEIVED: {}", parsed_data);
                        return Ok(ConnectionActions::StartConnection(parsed_data));
                    }
                    Err(_) => {
                        return Ok(ConnectionActions::Disconnect(true));
                    }
                }
            }
            Err(_) => {
                return Ok(ConnectionActions::Disconnect(true));
            }
        }
    }

    fn get_serial_number() -> io::Result<String> {
        let path = Path::new("/proc/cpuinfo");
        let file = File::open(&path)?;
        let reader = io::BufReader::new(file);

        for line in reader.lines() {
            let line = line?;
            if line.starts_with("Serial") {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() == 2 {
                    return Ok(parts[1].trim().to_string());
                }
            }
        }

        Err(io::Error::new(io::ErrorKind::NotFound, "Serial number not found"))
    }
}
