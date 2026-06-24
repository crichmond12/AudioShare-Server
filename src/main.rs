//mod authentication;
//mod mdb;
//use std::sync::Arc;
mod audio;
mod server;
mod errors;
mod rest_server;
mod session;
mod secure_error;
mod security;
mod mdb;
mod json_structs;
mod pairing;
//mod connection;
use std::sync::Arc;
/*mod server;
mod connection;
mod broadcast;
use broadcast::Broadcast;
use connection::ConnectServer;
*/
//use server::Server;

#[tokio::main]
async fn main() {
    //let controller = Arc::new(Controller::new());
    /*if let Err(err) = periodic_broadcast(interval_seconds).await {
        eprintln!("Error in periodic broadcasting: {}", err);
    }*/
    //let rest_server = RestServer::new();
    //rest_server.start().await;
    let server = Arc::new(server::server::Server::new());

    // Turn on AirPlay receiving: one classic shairport-sync per zone, routed through
    // the engine. If shairport-sync isn't installed, individual spawns just log and
    // are skipped (the rest of the server still runs).
    {
        use audio::engine::ENGINE;
        use audio::airplay_factory::ShairportReceiverFactory;

        let engine_ref: &'static audio::engine::Engine = &ENGINE;
        let sessions: std::sync::Arc<dyn audio::engine::SessionSink> =
            std::sync::Arc::new(engine_ref);
        ENGINE.enable_airplay(Box::new(ShairportReceiverFactory::new(sessions)));
    }

    server.start().await;
    // Create a daemon
    }


