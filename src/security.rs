mod mdb;
use curve25519_dalek::{ristretto::RistrettoPoint, scalar::Scalar};
use rand::rngs::OsRng;
use mdb::MDB;
use rusqlite::{Connection};

pub struct Security {
    public_key: [u8; 32],
    private_key: [u8, 32],
    MDB: MDB,
}

impl Security {
    pub fn new -> Self {
        Self {
            MDB: MDB::new(), 
        }
    }
}
