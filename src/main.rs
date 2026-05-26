//mod authentication;
//mod mdb;
//use std::sync::Arc;
mod server;
mod errors;
mod rest_server;
mod session;
mod secure_error;
mod security;
mod mdb;
mod json_structs;
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
    server.start().await;
    // Create a daemon
    }


