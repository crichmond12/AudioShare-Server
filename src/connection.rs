use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use local_ip_address::local_ip;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
//use std::net::{TcpListener, TcpStream};
use std::thread;
use tokio::time::{interval, Duration};
use tokio::net::TcpListener;
use tokio::task;



//use crate::authentication::generate_key_pair;
//use crate::mdb::MDB;
use rusqlite::{Connection};

pub struct ConnectServer {
    pub serial_number: String,
    pub ip: String,
    pub server_port: i32,
    //pub MDB: Connection,
    }

impl ConnectServer {
    pub fn new() -> Self{
        let my_ip_address = local_ip().unwrap().to_string();
         match Self::get_serial_number() {
            Ok(serial_number) => {
                Self {
                    serial_number: serial_number,
                    ip:  my_ip_address, 
                    server_port:  50505,
                }
            }
            Err(_) => Self {
                serial_number: "null".to_string(),
                ip: "null".to_string(),
                server_port: 0,
                },
        }
    }

    async fn handle_client(&self, mut stream:tokio::net::TcpStream) -> Result<Vec<u8>, std::io::Error>{
        let mut buffer = vec![0; 1024];

        loop {
            // Read data from the socket
            let n = stream.read(&mut buffer).await?;

            // If the read returns 0, it means the connection was closed
            if n == 0 {
                break;
            }

            // Print the received data
            println!("Received: {}", String::from_utf8_lossy(&buffer[..n]));

            // Echo the data back to the client
            stream.write_all(&buffer[..n]).await?;
        }

        Ok((buffer))
        /*let mut buffer = [0; 512];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break, // Connection closed
                Ok(n) => {
                    // Echo back the received data
                    if stream.write_all(&buffer[0..n]).is_err() {
                        break;
                    }
                    println!("Success");
                }
                Err(_) => break,
            }
        }*
        let mut buffer = vec![0; 1024]; // Adjust buffer size as needed

        // Read from the stream
        println!("Waiting");
        let bytes_read = stream.read_to_end(&mut buffer).await?;

        //let bytes_read = stream.read(&mut buffer).await?;

        // Trim buffer to actual bytes read
        buffer.resize(bytes_read, 0);
        println!("{}", buffer);

        Ok(buffer)*/
    }

    pub async fn start_server(self: Arc<Self>) {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", self.server_port)).await.expect("Failed to bind TCP listener");
        println!("Server listening on port {}", self.server_port);

        // Accept incoming connections
         loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let server = Arc::clone(&self);
                    println!("CONNECTING");
                    task::spawn(async move {
                        server.handle_client(stream).await;
                    });
                }
                Err(e) => {
                    println!("Connection failed: {}", e);
                }
            }
        }
        /*for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    // Handle each connection in a new thread
                    let server = Arc::clone(&self);

                    thread::spawn(move async {
                        //let server = server;
                        server.handle_client(stream).await;
                        });
                    }
                    Err(e) => {
                        println!("Connection failed: {}", e);
                    }
                }
            }*/
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

