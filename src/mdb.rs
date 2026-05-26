use rusqlite::{Connection, Result};
use crate::errors::connection_error::ConnectionError;

#[allow(dead_code)]
pub struct MDB {
    conn: Connection,
}

#[allow(dead_code)]
impl MDB {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        match Connection::open("audio_share.db") {
            Ok(connection) => {
                let new_mdb = Self {
                    conn: connection,
                };
                return Ok(new_mdb);
            }
            Err(_) => {
                return Err(Box::new(ConnectionError::new("Error connecting to audioshare database")));
            }
        }
    }

    pub fn q(&self, sql: &str, _todb: Vec<Box<dyn std::any::Any>>) -> Result<(), Box<dyn std::error::Error>> {
        let _stmt = self.conn.prepare(sql);
        Ok(())
    }
}
