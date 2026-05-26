use rand::rngs::OsRng;
use crate::session::Session;
use x25519_dalek::{PublicKey, EphemeralSecret};
use hkdf::Hkdf;
use sha2::Sha256;
use aes_gcm::{Aes256Gcm, Key, Nonce, KeyInit};
use aes_gcm::aead::Aead;
use rand::Rng;
use crate::errors::connection_error::ConnectionError;
use base64::Engine as _;
use base64::engine::general_purpose;

#[derive(Clone)]
pub struct Security {
    session: Session,
    client_public_key: PublicKey,
}

impl Security {
    pub fn new(client_public_key: PublicKey) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            session: Session::new(client_public_key),
            client_public_key,
        })
    }

    #[allow(dead_code)]
    pub fn get_session(&self) -> Session {
        self.session
    }

    pub fn touch_session(&mut self) {
        self.session.last_activity = std::time::Instant::now();
    }

    pub fn get_public_key_from_request(request: serde_json::Value) -> Result<PublicKey, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(base64_pubkey) = request["public_key"].as_str() {
            let client_public_key_bytes = general_purpose::STANDARD.decode(base64_pubkey)?;
            let client_public_key_bytes_arr: [u8; 32] = client_public_key_bytes.as_slice().try_into()?;
            return Ok(PublicKey::from(client_public_key_bytes_arr));
        }

        Err(Box::new(ConnectionError::new("Error finding public key in request.")))
    }

    pub fn get_encrypted_session_key(&self) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        // Generate an ephemeral key for the encryption process
        let server_private_key = EphemeralSecret::random_from_rng(OsRng);
        let server_public_key = PublicKey::from(&server_private_key);

        // Derive shared secret
        let shared_secret = server_private_key.diffie_hellman(&self.client_public_key);

        // Use HKDF to derive an encryption key from the shared secret
        let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
        let mut encryption_key = [0u8; 32];
        hk.expand(&[], &mut encryption_key).expect("HKDF expand failed");

        let key = Key::<Aes256Gcm>::from_slice(&encryption_key);
        let cipher = Aes256Gcm::new(key);
        let nonce: [u8; 12] = rand::thread_rng().gen();
        let nonce = Nonce::from_slice(&nonce); // 96-bits; unique per message
        let ciphertext = cipher
            .encrypt(nonce, self.session.get_session_key_slice())
            .map_err(|_| Box::new(ConnectionError::new("No Public Key In Request")))?;

        // Combine nonce, ciphertext, and server public key into one buffer
        let mut encrypted_message = Vec::new();
        encrypted_message.extend_from_slice(nonce);
        encrypted_message.extend_from_slice(&ciphertext);
        encrypted_message.extend_from_slice(server_public_key.as_bytes());

        Ok(encrypted_message)
    }

    #[allow(dead_code)]
    pub fn encrypt_data(&self, data: String) -> String {
        // Create AES-GCM cipher instance
        let session_key = self.session.get_session_key_slice();
        let key = Key::<Aes256Gcm>::from_slice(session_key);
        let cipher = Aes256Gcm::new(key);

        // Generate a random nonce
        let nonce: [u8; 12] = rand::thread_rng().gen();
        let nonce = Nonce::from_slice(&nonce); // 96-bits; unique per message

        // Encrypt the plaintext
        let _ciphertext = cipher.encrypt(nonce, data.as_bytes());
        let return_val = "work";
        return_val.to_string()
    }

    pub fn decrypt_data(&self, data: String) -> Result<String, &'static str> {
        // Decode the base64 encoded string
        let decoded_data = general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "Base64 decoding failed")?;

        // Split the nonce and ciphertext
        if decoded_data.len() < 12 {
            return Err("Invalid data");
        }
        let (nonce, ciphertext) = decoded_data.split_at(12);

        // Create AES-GCM cipher instance
        let session_key = self.session.get_session_key_slice();
        let key = Key::<Aes256Gcm>::from_slice(session_key);
        let cipher = Aes256Gcm::new(key);

        // Decrypt the ciphertext
        let decrypted_data = cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| "Decryption failed")?;

        // Convert decrypted data to string
        let decrypted_string =
            String::from_utf8(decrypted_data).map_err(|_| "UTF-8 conversion failed")?;
        Ok(decrypted_string)
    }
}
