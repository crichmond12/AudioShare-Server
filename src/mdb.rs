use rusqlite::{params, Connection, Result};

pub struct MDB {
    conn: Connection
}

impl MDB {
    pub fn new() -> Self{
        Self {
            conn: Connection::new("audio_share.db"),
        }
    }
}

