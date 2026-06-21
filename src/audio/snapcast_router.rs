//! Hub-driven Snapcast stream pool, group reconciler, and router
//! (multi-room Change 5, sub-step 3).
//!
//! The engine's single seam into Snapcast. It owns a fixed pool of `snapserver`
//! pipe streams (`StreamPool`), allocates one to each *playing* dongle zone, and
//! reconciles `snapserver`'s groups/streams to the hub's desired topology over
//! the control API. See `docs/multi-room-plan.md` Change 5 sub-step 3.

use std::sync::Arc;

use crate::audio::sink::AudioSink;
use crate::audio::snapcast::{fifo_path, stream_id, SnapcastSink};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
