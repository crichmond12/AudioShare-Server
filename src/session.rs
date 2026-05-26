use std::time::Instant;
use x25519_dalek::{PublicKey, EphemeralSecret};
use rand::rngs::OsRng;
use hkdf::Hkdf;
use sha2::Sha256;

#[derive(Copy, Clone)]
pub struct Session {
    session_key: [u8; 32],
    pub last_activity: Instant,
}

impl Session {
    pub fn new(client_public_key: PublicKey) -> Self {
        // Generate a new private key for this connection
        let server_private_key = EphemeralSecret::random_from_rng(OsRng);
        let _server_public_key = PublicKey::from(&server_private_key);

        // Derive the shared secret
        let shared_secret = server_private_key.diffie_hellman(&client_public_key);

        // Use HKDF to derive a symmetric key from the shared secret
        let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
        let mut session_key = [0u8; 32]; // 256-bit key
        hk.expand(&[], &mut session_key).expect("HKDF expand failed");

        Self {
            session_key: session_key,
            last_activity: Instant::now(),
        }
    }

    #[allow(dead_code)]
    pub fn get_session_key(&self) -> [u8; 32] {
        self.session_key
    }

    pub fn get_session_key_slice(&self) -> &[u8] {
        &self.session_key
    }
}
