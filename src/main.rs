//mod authentication;
//mod mdb;
//use std::sync::Arc;
use std::sync::Arc;
mod connection;
mod broadcast;
use broadcast::Broadcast;
use connection::ConnectServer;

#[tokio::main]
async fn main() {
    //let controller = Arc::new(Controller::new());
    /*if let Err(err) = periodic_broadcast(interval_seconds).await {
        eprintln!("Error in periodic broadcasting: {}", err);
    }*/
    let server = Arc::new(connection::ConnectServer::new());
    let broadcaster = Arc::new(broadcast::Broadcast::new());


    let server_handle = tokio::spawn(async move {
        let connect_server = Arc::clone(&server);//ConnectServer::new();
        connect_server.start_server().await;
    });

    let broadcast_handle = tokio::spawn(async move {
        //let broadcast = Broadcast::new();
        let broadcast = Arc::clone(&broadcaster);
        broadcast.startBroadcast().await;
    });
    
    let _ = tokio::try_join!(broadcast_handle, server_handle);

    // Create a daemon
    }


