//! Persistent dongle state (multi-room Change 5, sub-step 2.3).
//!
//! A dongle needs two facts to survive reboots:
//! - its **identity** — a UUID generated once and reused forever (it becomes the
//!   hub's `OutputId`) plus a human `name`; and
//! - its **assigned hub** — the `host:port` the app handed it via
//!   [`AppToDongle::Assign`](audioshare_protocol::AppToDongle), so it reconnects
//!   to *that* hub on every boot instead of re-discovering one.
//!
//! This mirrors the hub's `pairing.rs` (`load_or_create` + atomic write): plain
//! files under the user data dir so the agent runs unprivileged. [`Storage`]
//! wraps a directory; production uses [`Storage::new`] (the XDG path) and tests
//! use [`Storage::at`] (a temp dir), which keeps the persistence logic testable
//! without touching the real data dir.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use uuid::Uuid;

use audioshare_protocol::HUB_REGISTRATION_PORT;

const ID_FILE: &str = "dongle_id";
const NAME_FILE: &str = "dongle_name";
const HUB_FILE: &str = "hub_address";

/// A dongle's stable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Persisted UUID; the hub uses this as the `OutputId`.
    pub id: String,
    /// Human label (defaults to the hostname); the hub shows it as the output name.
    pub name: String,
}

/// The hub a dongle has been assigned to (its registration listener address).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubAddress {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for HubAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl FromStr for HubAddress {
    type Err = String;

    /// Parse `host` or `host:port`; the port defaults to [`HUB_REGISTRATION_PORT`]
    /// so a bare IP works for the `--hub` dev shortcut. IPv6 literals are not
    /// supported (LAN dongles use IPv4); a value with multiple colons is rejected.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty hub address".to_string());
        }
        match s.rsplit_once(':') {
            Some((host, port)) => {
                if host.is_empty() {
                    return Err(format!("hub address missing host: {s:?}"));
                }
                if host.contains(':') {
                    return Err(format!("IPv6 hub addresses are not supported: {s:?}"));
                }
                let port = port
                    .parse::<u16>()
                    .map_err(|_| format!("invalid hub port in {s:?}"))?;
                Ok(HubAddress {
                    host: host.to_string(),
                    port,
                })
            }
            None => Ok(HubAddress {
                host: s.to_string(),
                port: HUB_REGISTRATION_PORT,
            }),
        }
    }
}

/// Persistence rooted at one directory.
pub struct Storage {
    dir: PathBuf,
}

impl Storage {
    /// Production storage under the user data dir (honors `$XDG_DATA_HOME`,
    /// falling back to `~/.local/share`, then the system temp dir). A dedicated
    /// `audioshare_dongle` subdir so it never collides with the hub's
    /// `audio_share` dir when both run on one dev machine.
    pub fn new() -> Self {
        Self { dir: data_dir() }
    }

    /// Storage rooted at an explicit directory (used by tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Load the persisted identity, creating + persisting it on first run.
    ///
    /// The UUID is generated once and reused. The name resolves as
    /// `name_override` → persisted name → system hostname → a static fallback,
    /// and the resolved value is persisted so the choice is stable.
    pub fn load_or_create_identity(&self, name_override: Option<String>) -> io::Result<Identity> {
        fs::create_dir_all(&self.dir)?;

        let id_path = self.dir.join(ID_FILE);
        let id = if id_path.exists() {
            fs::read_to_string(&id_path)?.trim().to_string()
        } else {
            let id = Uuid::new_v4().to_string();
            self.atomic_write(ID_FILE, &id)?;
            id
        };

        let name_path = self.dir.join(NAME_FILE);
        let name = match name_override {
            Some(name) => {
                let name = name.trim().to_string();
                self.atomic_write(NAME_FILE, &name)?;
                name
            }
            None if name_path.exists() => fs::read_to_string(&name_path)?.trim().to_string(),
            None => {
                let name = default_name();
                self.atomic_write(NAME_FILE, &name)?;
                name
            }
        };

        Ok(Identity { id, name })
    }

    /// Load the assigned hub address, or `None` if the dongle is still unassigned.
    pub fn load_hub(&self) -> Option<HubAddress> {
        let raw = fs::read_to_string(self.dir.join(HUB_FILE)).ok()?;
        raw.parse().ok()
    }

    /// Persist the assigned hub address.
    pub fn save_hub(&self, hub: &HubAddress) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        self.atomic_write(HUB_FILE, &hub.to_string())
    }

    /// Write `contents` to `<dir>/<file>` atomically (write `.tmp`, then rename),
    /// matching the hub's `pairing.rs` so a crash never leaves a half-written file.
    fn atomic_write(&self, file: &str, contents: &str) -> io::Result<()> {
        let path = self.dir.join(file);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, contents)?;
        fs::rename(&tmp, &path)
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self::new()
    }
}

/// User data dir for dongle state. See [`Storage::new`].
fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("audioshare_dongle")
}

/// The system hostname, used as the default dongle name. Shelling out to
/// `hostname` keeps the agent dependency-light and works on both Linux (the
/// flashed dongle) and macOS (dev); falls back to a static label if it fails.
fn default_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "audioshare-dongle".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "audioshare-dongle-test-{tag}-{}",
            Uuid::new_v4()
        ));
        dir
    }

    #[test]
    fn identity_is_created_then_reused() {
        let dir = temp_dir("identity");
        let storage = Storage::at(&dir);

        let first = storage.load_or_create_identity(None).expect("create");
        assert!(!first.id.is_empty());
        let second = storage.load_or_create_identity(None).expect("reload");
        assert_eq!(first.id, second.id, "the UUID must persist across runs");
        assert_eq!(first.name, second.name);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn name_override_persists_and_wins() {
        let dir = temp_dir("name");
        let storage = Storage::at(&dir);

        let named = storage
            .load_or_create_identity(Some("Kitchen".to_string()))
            .expect("create with name");
        assert_eq!(named.name, "Kitchen");

        // A later run without an override keeps the persisted name.
        let reloaded = storage.load_or_create_identity(None).expect("reload");
        assert_eq!(reloaded.name, "Kitchen");
        assert_eq!(reloaded.id, named.id);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hub_address_round_trips_through_disk() {
        let dir = temp_dir("hub");
        let storage = Storage::at(&dir);

        assert_eq!(storage.load_hub(), None, "unassigned by default");

        let hub = HubAddress {
            host: "192.168.1.10".to_string(),
            port: 50506,
        };
        storage.save_hub(&hub).expect("save");
        assert_eq!(storage.load_hub(), Some(hub));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hub_address_parses_bare_host_and_host_port() {
        let bare: HubAddress = "10.0.0.5".parse().unwrap();
        assert_eq!(bare.host, "10.0.0.5");
        assert_eq!(bare.port, HUB_REGISTRATION_PORT);

        let full: HubAddress = "10.0.0.5:50506".parse().unwrap();
        assert_eq!(full.host, "10.0.0.5");
        assert_eq!(full.port, 50506);

        assert!("10.0.0.5:notaport".parse::<HubAddress>().is_err());
        assert!("".parse::<HubAddress>().is_err());
        assert!(":50506".parse::<HubAddress>().is_err());
    }
}
