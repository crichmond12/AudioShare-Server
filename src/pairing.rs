use std::fs;
use std::path::{Path, PathBuf};
use rand::RngCore;
use rand::rngs::OsRng;
use base64::engine::general_purpose;
use base64::Engine as _;

/// On-disk location of the pairing secret.
///
/// Kept in the user's data directory so the service runs unprivileged. The
/// previous `/etc/audio_share/` path required root to create, which broke
/// `cargo run` as a normal user on the Pi. Honors `$XDG_DATA_HOME`, falling
/// back to `~/.local/share`, then `/tmp` as a last resort.
#[cfg(target_os = "linux")]
pub fn pairing_secret_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("audio_share").join("pairing_secret.b64")
}

#[cfg(not(target_os = "linux"))]
pub fn pairing_secret_path() -> PathBuf {
    std::env::temp_dir().join("audio_share_pairing_secret.b64")
}

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

/// On the headless Pi there's no display, so render the QR as Unicode
/// half-blocks straight to the terminal (e.g. the operator's SSH session) where
/// the iOS app can scan it. Colors are inverted (light modules on a dark
/// background) per the `qrcode` crate's terminal recipe so it scans on the
/// typical dark terminal. Falls back to the raw payload if rendering fails.
#[cfg(not(target_os = "macos"))]
pub fn present_qr(payload: &str) {
    use qrcode::QrCode;
    use qrcode::render::unicode;

    println!("=== SCAN THIS QR CODE TO PAIR ===");

    match QrCode::new(payload.as_bytes()) {
        Ok(code) => {
            let rendered = code
                .render::<unicode::Dense1x2>()
                .dark_color(unicode::Dense1x2::Light)
                .light_color(unicode::Dense1x2::Dark)
                .quiet_zone(true)
                .build();
            println!("{}", rendered);
        }
        Err(e) => eprintln!("Could not generate QR code (scan the payload below): {}", e),
    }

    println!("{}", payload);
    println!("=================================");
}
