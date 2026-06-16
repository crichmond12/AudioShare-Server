use mdns_sd::{ServiceDaemon, ServiceInfo};
use local_ip_address::local_ip;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::io::BufRead;
#[cfg(target_os = "linux")]
use std::path::Path;
use tokio::time::{interval, Duration};

pub struct Broadcast {
    pub serial_number: String,
    pub ip: String,
    pub server_port: i32,
    //pub MDB: Connection,
    }

impl Broadcast {
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

    pub async fn start_broadcast(&self) {
        let mdns = match self.register_service() {
            Ok(mdns) => mdns,
            Err(err) => {
                eprintln!("Failed to register mDNS service: {}", err);
                return;
            }
        };

        let mut interval = interval(Duration::from_secs(60 * 60));

        loop {
            interval.tick().await;
            let _ = &mdns;
        }
    }

    fn register_service(&self) -> Result<ServiceDaemon, Box<dyn std::error::Error + Send + Sync>> {
        let mdns = ServiceDaemon::new()?;
        let service_type = "_audioshare._tcp.local.";
        let instance_name = "AudioShare Device";
        let host_name = self.ip.clone() + ".local.";
        let port = self.server_port as u16;
        let properties = [("serial_number", self.serial_number.to_string())];

        let my_service = ServiceInfo::new(
            service_type,
            instance_name,
            &host_name,
            self.ip.to_string(),
            port,
            &properties[..],
        )?;

        mdns.register(my_service)?;
        Ok(mdns)
    }

    #[cfg(not(target_os = "linux"))]
    fn get_serial_number() -> io::Result<String> {
        Ok("dev-mac-serial".to_string())
    }

    #[cfg(target_os = "linux")]
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
