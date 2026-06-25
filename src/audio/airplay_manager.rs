//! AirPlay receiver supervision (Phase 4, Slice 2).
//!
//! [`ShairportManager`] keeps one classic `shairport-sync` receiver per zone,
//! reconciling against the live zone set the same way the Snapcast reconciler
//! converges snapserver: spawn receivers for new zones, kill them for removed
//! zones, restart-renamed for renamed zones. Spawning is behind the
//! [`ReceiverFactory`]/[`ZoneReceiver`] seam so the diff is unit-tested with a
//! fake — no `shairport-sync`, no audio. The production factory lives in
//! `audio::airplay_factory`.

use std::collections::HashMap;
use std::sync::Mutex;

/// A live AirPlay receiver for one zone (a supervised `shairport-sync` + its PCM
/// pump thread). Dropping it tears both down.
pub trait ZoneReceiver: Send + Sync {
    /// Restart the receiver advertising `new_name` (the mDNS/AirPlay name).
    fn rename(&self, new_name: &str) -> Result<(), String>;
}

/// Creates [`ZoneReceiver`]s. The production impl spawns `shairport-sync`; tests
/// use a fake that records calls.
pub trait ReceiverFactory: Send + Sync {
    fn create(&self, zone: &str, name: &str, slot: usize) -> Result<Box<dyn ZoneReceiver>, String>;
}

struct Managed {
    name: String,
    slot: usize,
    receiver: Box<dyn ZoneReceiver>,
}

/// Owns the per-zone receivers and converges them to the desired zone set.
pub struct ShairportManager {
    factory: Box<dyn ReceiverFactory>,
    receivers: Mutex<HashMap<String, Managed>>, // keyed by zone id
}

impl ShairportManager {
    pub fn new(factory: Box<dyn ReceiverFactory>) -> Self {
        Self { factory, receivers: Mutex::new(HashMap::new()) }
    }

    /// Converge receivers to `desired` (zone_id, name): spawn missing, kill
    /// removed, rename changed. Idempotent. Slot assignment is lowest-free so the
    /// port/device-id pool stays small and stable.
    pub fn reconcile(&self, desired: &[(String, String)]) {
        let mut receivers = self.receivers.lock().expect("airplay receivers mutex poisoned");

        // Kill receivers whose zone is gone (drop runs the receiver's teardown).
        let desired_ids: std::collections::HashSet<&str> =
            desired.iter().map(|(id, _)| id.as_str()).collect();
        receivers.retain(|id, _| desired_ids.contains(id.as_str()));

        for (id, name) in desired {
            match receivers.get_mut(id) {
                Some(m) => {
                    if &m.name != name {
                        if m.receiver.rename(name).is_ok() {
                            m.name = name.clone();
                        }
                    }
                }
                None => {
                    let slot = lowest_free_slot(&receivers);
                    match self.factory.create(id, name, slot) {
                        Ok(receiver) => {
                            receivers.insert(id.clone(), Managed { name: name.clone(), slot, receiver });
                        }
                        Err(e) => {
                            eprintln!("airplay: failed to start receiver for zone {id}: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// Lowest slot index not currently in use by an existing receiver.
fn lowest_free_slot(receivers: &HashMap<String, Managed>) -> usize {
    let used: std::collections::HashSet<usize> = receivers.values().map(|m| m.slot).collect();
    (0..).find(|s| !used.contains(s)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Log { events: Mutex<Vec<String>> }

    struct FakeReceiver { zone: String, slot: usize, log: Arc<Log> }
    impl ZoneReceiver for FakeReceiver {
        fn rename(&self, new_name: &str) -> Result<(), String> {
            self.log.events.lock().unwrap().push(format!("rename {} -> {}", self.zone, new_name));
            Ok(())
        }
    }
    impl Drop for FakeReceiver {
        fn drop(&mut self) {
            self.log.events.lock().unwrap().push(format!("kill {} slot{}", self.zone, self.slot));
        }
    }

    struct FakeFactory { log: Arc<Log> }
    impl ReceiverFactory for FakeFactory {
        fn create(&self, zone: &str, name: &str, slot: usize) -> Result<Box<dyn ZoneReceiver>, String> {
            self.log.events.lock().unwrap().push(format!("create {} '{}' slot{}", zone, name, slot));
            Ok(Box::new(FakeReceiver { zone: zone.to_string(), slot, log: Arc::clone(&self.log) }))
        }
    }

    fn drain(log: &Arc<Log>) -> Vec<String> { std::mem::take(&mut *log.events.lock().unwrap()) }

    #[test]
    fn reconcile_spawns_new_zones_and_assigns_slots() {
        let log = Arc::new(Log::default());
        let mgr = ShairportManager::new(Box::new(FakeFactory { log: Arc::clone(&log) }));

        mgr.reconcile(&[("default".into(), "Hub".into()), ("d1".into(), "Kitchen".into())]);
        let mut got = drain(&log);
        got.sort();
        assert_eq!(got, vec![
            "create d1 'Kitchen' slot1".to_string(),
            "create default 'Hub' slot0".to_string(),
        ]);

        // Reconciling the same set is a no-op (idempotent).
        mgr.reconcile(&[("default".into(), "Hub".into()), ("d1".into(), "Kitchen".into())]);
        assert!(drain(&log).is_empty());
    }

    #[test]
    fn reconcile_kills_removed_and_renames_changed() {
        let log = Arc::new(Log::default());
        let mgr = ShairportManager::new(Box::new(FakeFactory { log: Arc::clone(&log) }));
        mgr.reconcile(&[("d1".into(), "Kitchen".into())]);
        drain(&log);

        // Rename d1, remove nothing.
        mgr.reconcile(&[("d1".into(), "Cucina".into())]);
        assert_eq!(drain(&log), vec!["rename d1 -> Cucina".to_string()]);

        // Remove d1 entirely -> its receiver is dropped (kill).
        mgr.reconcile(&[]);
        assert_eq!(drain(&log), vec!["kill d1 slot0".to_string()]);

        // A new zone reuses the now-free slot 0.
        mgr.reconcile(&[("d2".into(), "Bath".into())]);
        assert_eq!(drain(&log), vec!["create d2 'Bath' slot0".to_string()]);
    }
}
