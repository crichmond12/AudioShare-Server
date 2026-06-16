use std::fs;
use std::path::Path;
use rand::RngCore;
use rand::rngs::OsRng;
use base64::engine::general_purpose;
use base64::Engine as _;

#[cfg(target_os = "linux")]
pub const PAIRING_SECRET_PATH: &str = "/etc/audio_share/pairing_secret.b64";

#[cfg(not(target_os = "linux"))]
pub const PAIRING_SECRET_PATH: &str = "/tmp/audio_share_pairing_secret.b64";

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

/// Present the pairing payload to the operator.
///
/// On macOS (development) this renders the payload to a QR PNG and opens it in
/// Preview so it can be scanned with the iOS app. On other platforms (the Pi)
/// it just prints the payload text, since the device is headless.
#[cfg(target_os = "macos")]
pub fn present_qr(payload: &str) {
    use qrcode::QrCode;
    use image::Luma;

    println!("=== SCAN THIS QR CODE TO PAIR ===");
    println!("{}", payload);
    println!("=================================");

    match QrCode::new(payload.as_bytes()) {
        Ok(code) => {
            let image = code
                .render::<Luma<u8>>()
                .min_dimensions(320, 320)
                .build();
            let path = std::env::temp_dir().join("audio_share_qr.png");
            if let Err(e) = image.save(&path) {
                eprintln!("Could not save QR PNG: {}", e);
                return;
            }
            println!("QR code image saved to {}", path.display());
            if let Err(e) = std::process::Command::new("open").arg(&path).spawn() {
                eprintln!("Could not open QR PNG (open it manually): {}", e);
            }
        }
        Err(e) => eprintln!("Could not generate QR code: {}", e),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn present_qr(payload: &str) {
    println!("=== SCAN THIS QR CODE TO PAIR ===");
    println!("{}", payload);
    println!("=================================");
}
