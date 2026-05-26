use crate::security::Security;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use lazy_static::lazy_static;
use std::time::{Duration, Instant};
use uuid::Uuid;
use crate::server::{connection_server, broadcast};

pub struct Server {
    sessions: Arc<Mutex<HashMap<Uuid, Security>>>,
}

impl Server {
    pub fn new() -> Self {
        Server {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(self: Arc<Self>) {
        let connection_server = Arc::new(connection_server::ConnectServer::new());
        let broadcaster = Arc::new(broadcast::Broadcast::new());

        let server_handle = tokio::spawn(async move {
            let connect_server = Arc::clone(&connection_server);
            connect_server.start_server().await;
        });

        let broadcast_handle = tokio::spawn(async move {
            let broadcast = Arc::clone(&broadcaster);
            broadcast.start_broadcast().await;
        });

        let _ = tokio::try_join!(broadcast_handle, server_handle);
    }

    pub async fn store_session(&self, client_uuid: Uuid, sec: Security) -> bool {
        let mut sessions = self.sessions.lock().await;
        sessions.insert(client_uuid, sec);
        true
    }

    #[allow(dead_code)]
    pub async fn get_symmetric_key(&self, client_uuid: &Uuid) -> Option<[u8; 32]> {
        let mut sessions = self.sessions.lock().await;
        if let Some(security) = sessions.get_mut(client_uuid) {
            security.touch_session();
            Some(security.get_session().get_session_key())
        } else {
            None
        }
    }

    pub async fn authenticate_session(&self, client_uuid: &Uuid) -> bool {
        let mut sessions = self.sessions.lock().await;
        if let Some(security) = sessions.get_mut(client_uuid) {
            security.touch_session();
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub async fn cleanup_expired_sessions(&self, idle_timeout: Duration) {
        let mut sessions = self.sessions.lock().await;
        let now = Instant::now();
        sessions.retain(|_, session| {
            now.duration_since(session.get_session().last_activity) < idle_timeout
        });
    }

    #[allow(dead_code)]
    pub fn get_active_sessions(&self) -> &tokio::sync::Mutex<HashMap<Uuid, Security>> {
        &self.sessions
    }
}

lazy_static! {
    pub static ref MAIN_SERVER: Arc<Mutex<Server>> = Arc::new(Mutex::new(Server::new()));
}

#[allow(dead_code)]
fn get_main_server() -> Arc<Mutex<Server>> {
    Arc::clone(&MAIN_SERVER)
}
