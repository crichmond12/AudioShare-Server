use std::fs;
use std::path::Path;
use rand::RngCore;
use rand::rngs::OsRng;
use base64::engine::general_purpose;
use base64::Engine as _;

pub const PAIRING_SECRET_PATH: &str = "/etc/audio_share/pairing_secret.b64";

pub fn load_or_create(path: &Path) -> Result<[u8; 32], Box<dyn std::error::Error + Send + Sync>> {
    if path.exists() {
        let encoded = fs::read_to_string(path)?.trim().to_string();
        let bytes = general_purpose::STANDARD.decode(&encoded)?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| "Stored pairing secret is not 32 bytes")?;
        return Ok(arr);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut secret = [0u8; 32];
    OsRng.fill_bytes(&mut secret);
    let encoded = general_purpose::STANDARD.encode(&secret);

    // Atomic write: write to .tmp then rename
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &encoded)?;
    fs::rename(&tmp, path)?;

    Ok(secret)
}

pub fn qr_payload(serial_number: &str, pairing_secret: &[u8; 32]) -> String {
    let ps_b64 = general_purpose::STANDARD.encode(pairing_secret);
    serde_json::json!({ "s": serial_number, "ps": ps_b64 }).to_string()
}
