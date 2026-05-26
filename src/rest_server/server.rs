use warp::Filter;
use crate::rest_server::spotify_routes;
use local_ip_address::local_ip;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use env_logger;

#[allow(dead_code)]
pub struct RestServer {
    running: bool,
}

#[allow(dead_code)]
impl RestServer {
    pub fn new() -> Self {
        Self {
            running: false,
        }
    }

    pub async fn start(&self) {
        env_logger::init();
        let log = warp::log("warp::server");
        let routes = spotify_routes::get_routes().with(log);

        let _ip_address = local_ip().unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 54762);

        println!("Starting Warp server on {}", addr);

        warp::serve(routes).run(addr).await;
    }
}
