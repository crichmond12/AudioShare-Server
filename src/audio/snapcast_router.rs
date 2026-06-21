//! Hub-driven Snapcast stream pool, group reconciler, and router
//! (multi-room Change 5, sub-step 3).
//!
//! The engine's single seam into Snapcast. It owns a fixed pool of `snapserver`
//! pipe streams (`StreamPool`), allocates one to each *playing* dongle zone, and
//! reconciles `snapserver`'s groups/streams to the hub's desired topology over
//! the control API. See `docs/multi-room-plan.md` Change 5 sub-step 3.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::audio::sink::AudioSink;
use crate::audio::snapcast::{fifo_path, stream_id, SnapcastSink};
use crate::audio::snapcast_control::{CommandConn, EventListener, SnapcastControl};

/// Host snapserver's control API listens on (local to the hub).
const CONTROL_HOST: &str = "127.0.0.1";
const CONTROL_PORT: u16 = 1705;

/// Concurrent *playing* zones the hub supports (the snapserver stream pool size).
/// Creating zones is unbounded; only playback consumes a slot.
pub const STREAM_POOL_SIZE: usize = 16;

/// A stream handed to a zone: the snapserver `stream_id` to bind its group to,
/// and the sink the decode thread writes into.
pub struct AllocatedStream {
    pub stream_id: String,
    pub sink: Arc<dyn AudioSink>,
}

struct Slot {
    stream_id: String,
    sink: Arc<SnapcastSink>,
    allocated_to: Option<String>,
}

/// One zone's desired Snapcast routing: the stream its group should play and the
/// dongle client ids that should be in that group.
pub struct ZoneRouting {
    pub stream_id: String,
    pub clients: Vec<String>,
}

/// A fixed pool of snapserver pipe streams, allocated one-per-playing-zone.
pub struct StreamPool {
    slots: Vec<Slot>,
}

impl StreamPool {
    /// Build a pool of `size` slots, each backed by its own indexed FIFO sink.
    /// Constructs no I/O — the `SnapcastSink`s open their FIFOs lazily on write.
    pub fn new(size: usize) -> Self {
        let slots = (0..size)
            .map(|k| Slot {
                stream_id: stream_id(k),
                sink: Arc::new(SnapcastSink::new(fifo_path(k))),
                allocated_to: None,
            })
            .collect();
        Self { slots }
    }

    /// Reserve a stream for `zone`. Idempotent: a zone already holding a slot
    /// gets the same one back. Returns `None` only when every slot is taken.
    pub fn allocate(&mut self, zone: &str) -> Option<AllocatedStream> {
        if let Some(slot) = self.slots.iter().find(|s| s.allocated_to.as_deref() == Some(zone)) {
            return Some(AllocatedStream {
                stream_id: slot.stream_id.clone(),
                sink: Arc::clone(&slot.sink) as Arc<dyn AudioSink>,
            });
        }
        let slot = self.slots.iter_mut().find(|s| s.allocated_to.is_none())?;
        slot.allocated_to = Some(zone.to_string());
        Some(AllocatedStream {
            stream_id: slot.stream_id.clone(),
            sink: Arc::clone(&slot.sink) as Arc<dyn AudioSink>,
        })
    }

    /// Free `zone`'s slot, if any, for reuse. No-op if the zone holds none.
    pub fn release(&mut self, zone: &str) {
        for slot in &mut self.slots {
            if slot.allocated_to.as_deref() == Some(zone) {
                slot.allocated_to = None;
            }
        }
    }

    /// The `stream_id` currently allocated to `zone`, if any.
    pub fn stream_for(&self, zone: &str) -> Option<String> {
        self.slots
            .iter()
            .find(|s| s.allocated_to.as_deref() == Some(zone))
            .map(|s| s.stream_id.clone())
    }
}

/// Converge snapserver's groups/streams to `entries`. Idempotent: re-running with
/// the same desired state is a no-op-equivalent set of calls. For each zone whose
/// clients are (partly) connected, pull the present clients into one group and
/// bind that group to the zone's stream. Zones with no connected client yet are
/// skipped — a later client-connect notification re-triggers reconcile.
pub fn reconcile(control: &dyn SnapcastControl, entries: &[ZoneRouting]) -> Result<(), String> {
    let status = control.get_status()?;
    for entry in entries {
        let present: Vec<String> = entry
            .clients
            .iter()
            .filter(|c| status.is_connected(c))
            .cloned()
            .collect();
        let Some(first) = present.first() else { continue };
        let Some(group) = status.group_of(first) else { continue };
        control.set_group_clients(group, &present)?;
        control.set_group_stream(group, &entry.stream_id)?;
    }
    Ok(())
}

/// The engine's single seam into Snapcast: owns the supervised snapserver, the
/// stream pool, the control connection + event listener, and the desired routing
/// the reconciler converges snapserver to.
pub struct SnapcastRouter {
    started: Mutex<Option<Started>>,
    pool: Mutex<StreamPool>,
    /// zone -> dongle client ids that should be grouped on the zone's stream.
    desired: Mutex<HashMap<String, Vec<String>>>,
}

/// The running side, created lazily on the first `sink_for_zone`.
struct Started {
    _supervisor: crate::audio::snapcast::SnapserverSupervisor,
    control: Arc<dyn SnapcastControl>,
    _events: EventListener,
}

impl SnapcastRouter {
    pub fn new() -> Self {
        Self {
            started: Mutex::new(None),
            pool: Mutex::new(StreamPool::new(STREAM_POOL_SIZE)),
            desired: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate a stream for `zone`, record its desired grouping, kick a
    /// reconcile, and return the sink to decode into. Lazily starts snapserver +
    /// control on first use. Errors with `no_free_stream` when the pool is full.
    pub fn sink_for_zone(
        &self,
        zone: &str,
        dongle_ids: &[String],
    ) -> Result<Arc<dyn AudioSink>, String> {
        #[cfg(not(test))]
        self.ensure_started()?;

        let allocated = {
            let mut pool = self.pool.lock().expect("stream pool mutex poisoned");
            pool.allocate(zone)
                .ok_or_else(|| "no_free_stream".to_string())?
        };

        self.desired
            .lock()
            .expect("desired mutex poisoned")
            .insert(zone.to_string(), dongle_ids.to_vec());

        self.reconcile_now();
        Ok(allocated.sink)
    }

    /// Free `zone`'s stream and drop its desired routing.
    pub fn release_zone(&self, zone: &str) {
        self.pool
            .lock()
            .expect("stream pool mutex poisoned")
            .release(zone);
        self.desired
            .lock()
            .expect("desired mutex poisoned")
            .remove(zone);
    }

    /// Build current desired routing from desired state + pool, and reconcile
    /// snapserver to it. Safe to call from the event listener thread.
    pub fn reconcile_now(&self) {
        let control = {
            let guard = self.started.lock().expect("started mutex poisoned");
            match guard.as_ref() {
                Some(s) => Arc::clone(&s.control),
                None => return,
            }
        };
        let entries = self.entries();
        if let Err(e) = reconcile(control.as_ref(), &entries) {
            eprintln!("snapcast reconcile failed (will retry on next trigger): {e}");
        }
    }

    fn entries(&self) -> Vec<ZoneRouting> {
        let desired = self.desired.lock().expect("desired mutex poisoned");
        let pool = self.pool.lock().expect("stream pool mutex poisoned");
        desired
            .iter()
            .filter_map(|(zone, clients)| {
                pool.stream_for(zone).map(|stream_id| ZoneRouting {
                    stream_id,
                    clients: clients.clone(),
                })
            })
            .collect()
    }

    /// Spawn snapserver + open the control connection + start the event listener,
    /// once. Idempotent.
    fn ensure_started(&self) -> Result<(), String> {
        let mut guard = self.started.lock().expect("started mutex poisoned");
        if guard.is_some() {
            return Ok(());
        }
        let supervisor =
            crate::audio::snapcast::SnapserverSupervisor::spawn(STREAM_POOL_SIZE)?;
        // Give snapserver a moment to bind its control port before connecting.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let control: Arc<dyn SnapcastControl> =
            Arc::new(CommandConn::connect(CONTROL_HOST, CONTROL_PORT)?);
        let events = EventListener::spawn(CONTROL_HOST, CONTROL_PORT, || {
            crate::audio::engine::ENGINE.snapcast_on_notify();
        })?;
        *guard = Some(Started {
            _supervisor: supervisor,
            control,
            _events: events,
        });
        Ok(())
    }
}

impl Default for SnapcastRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl SnapcastRouter {
    /// A router that behaves as "started" with no real snapserver/control, so
    /// allocation + desired bookkeeping are exercised device-free.
    fn for_test() -> Self {
        Self::new()
    }

    fn desired_routing_for_test(&self) -> Vec<ZoneRouting> {
        self.entries()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::snapcast_control::{ServerStatus, GroupInfo, SnapcastControl};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockControl {
        status: ServerStatus,
        set_clients: StdMutex<Vec<(String, Vec<String>)>>,
        set_stream: StdMutex<Vec<(String, String)>>,
    }
    impl SnapcastControl for MockControl {
        fn get_status(&self) -> Result<ServerStatus, String> { Ok(self.status.clone()) }
        fn set_group_clients(&self, group: &str, clients: &[String]) -> Result<(), String> {
            self.set_clients.lock().unwrap().push((group.to_string(), clients.to_vec()));
            Ok(())
        }
        fn set_group_stream(&self, group: &str, stream: &str) -> Result<(), String> {
            self.set_stream.lock().unwrap().push((group.to_string(), stream.to_string()));
            Ok(())
        }
    }

    fn status_with(groups: Vec<GroupInfo>, connected: &[&str]) -> ServerStatus {
        // ServerStatus.connected is private; build it via parse for the test by
        // round-tripping a GetStatus-shaped value instead.
        let groups_json: Vec<_> = groups.iter().map(|g| serde_json::json!({
            "id": g.id, "stream_id": g.stream_id,
            "clients": g.clients.iter().map(|c| serde_json::json!({
                "id": c, "connected": connected.contains(&c.as_str())
            })).collect::<Vec<_>>()
        })).collect();
        crate::audio::snapcast_control::parse_server_status(
            &serde_json::json!({ "server": { "groups": groups_json } })
        ).unwrap()
    }

    #[test]
    fn allocate_reuses_the_same_slot_for_a_zone() {
        let mut pool = StreamPool::new(2);
        let a = pool.allocate("kitchen").expect("first alloc");
        let b = pool.allocate("kitchen").expect("re-alloc same zone");
        assert_eq!(a.stream_id, b.stream_id, "same zone keeps its slot");
        assert_eq!(pool.stream_for("kitchen").as_deref(), Some(a.stream_id.as_str()));
    }

    #[test]
    fn allocate_gives_distinct_streams_to_distinct_zones() {
        let mut pool = StreamPool::new(2);
        let a = pool.allocate("kitchen").unwrap();
        let b = pool.allocate("bedroom").unwrap();
        assert_ne!(a.stream_id, b.stream_id);
    }

    #[test]
    fn allocate_returns_none_when_exhausted() {
        let mut pool = StreamPool::new(1);
        assert!(pool.allocate("kitchen").is_some());
        assert!(pool.allocate("bedroom").is_none(), "pool of 1 has no slot left");
    }

    #[test]
    fn release_frees_the_slot_for_reuse() {
        let mut pool = StreamPool::new(1);
        let a = pool.allocate("kitchen").unwrap();
        pool.release("kitchen");
        assert!(pool.stream_for("kitchen").is_none());
        let b = pool.allocate("bedroom").expect("slot freed");
        assert_eq!(a.stream_id, b.stream_id, "the freed slot is reused");
    }

    #[test]
    fn reconcile_groups_present_clients_and_binds_stream() {
        let control = MockControl {
            status: status_with(vec![
                GroupInfo { id: "gA".into(), stream_id: "default".into(),
                            clients: vec!["d1".into()] },
                GroupInfo { id: "gB".into(), stream_id: "default".into(),
                            clients: vec!["d2".into()] },
            ], &["d1", "d2"]),
            ..Default::default()
        };
        let entries = vec![ZoneRouting {
            stream_id: "as-0".into(),
            clients: vec!["d1".into(), "d2".into()],
        }];

        reconcile(&control, &entries).expect("reconcile");

        // Both present clients pulled into d1's group, bound to as-0.
        assert_eq!(*control.set_clients.lock().unwrap(),
                   vec![("gA".to_string(), vec!["d1".to_string(), "d2".to_string()])]);
        assert_eq!(*control.set_stream.lock().unwrap(),
                   vec![("gA".to_string(), "as-0".to_string())]);
    }

    #[test]
    fn reconcile_skips_zone_with_no_connected_clients() {
        let control = MockControl {
            status: status_with(vec![
                GroupInfo { id: "gA".into(), stream_id: "default".into(),
                            clients: vec!["d1".into()] },
            ], &[]), // d1 not connected
            ..Default::default()
        };
        let entries = vec![ZoneRouting { stream_id: "as-0".into(), clients: vec!["d1".into()] }];

        reconcile(&control, &entries).expect("reconcile");
        assert!(control.set_clients.lock().unwrap().is_empty());
        assert!(control.set_stream.lock().unwrap().is_empty());
    }

    #[test]
    fn router_allocates_and_records_desired_routing() {
        // Test-only: a router whose snapserver/control are considered "absent" so
        // sink_for_zone allocates + records desired state without real I/O.
        let router = SnapcastRouter::for_test();

        let sink = router
            .sink_for_zone("kitchen", &["d1".to_string(), "d2".to_string()])
            .expect("alloc");
        assert_eq!(sink.sample_rate(), 48_000);

        let routing = router.desired_routing_for_test();
        assert_eq!(routing.len(), 1);
        assert_eq!(routing[0].clients, vec!["d1".to_string(), "d2".to_string()]);
        assert!(routing[0].stream_id.starts_with("as-"));

        router.release_zone("kitchen");
        assert!(router.desired_routing_for_test().is_empty());
    }
}
