//! Output registry (multi-room Change 2).
//!
//! An *output* is a destination the engine can route audio to: the local cpal
//! device, or — later — a network sink streaming to a remote receiver
//! ("dongle"). The [`OutputRegistry`] is the process-wide catalogue of these,
//! keyed by a stable [`OutputId`]. The local device registers itself lazily on
//! first playback (see [`crate::audio::engine`]); dongles will register
//! themselves when they connect over the network (Change 5).
//!
//! This is intentionally just a map behind a `Mutex` — no eventing/observers
//! yet. Zones (in `engine.rs`) reference outputs by id and resolve them to
//! sinks through here.

// `name` and the mutation/listing methods (`remove`, `set_online`, `contains`,
// `list`) are the registry's full surface; the engine uses `register`/`sink`
// today, and the rest is wired in by Change 4 (list outputs to the client) and
// Change 5 (dongle register/disconnect).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::audio::sink::AudioSink;

/// Stable identifier for an output. `"local"` is reserved for the host's own
/// cpal device; dongles use their own ids (e.g. a serial or user-chosen name).
pub type OutputId = String;

/// A registered audio destination.
pub struct Output {
    pub id: OutputId,
    /// Human-facing name / location, e.g. "Kitchen".
    pub name: String,
    /// Where PCM for this output goes. `Some` for the local device; `None` for
    /// dongle outputs, which are grouped in snapserver rather than decoded into
    /// individually.
    pub sink: Option<Arc<dyn AudioSink>>,
    /// Whether the output is currently reachable. Offline outputs stay in the
    /// registry (so their zone membership/name persists) but are skipped when
    /// resolving sinks for playback.
    pub online: bool,
}

/// Process-wide catalogue of outputs, keyed by [`OutputId`].
pub struct OutputRegistry {
    outputs: Mutex<HashMap<OutputId, Output>>,
}

impl OutputRegistry {
    pub fn new() -> Self {
        Self {
            outputs: Mutex::new(HashMap::new()),
        }
    }

    /// Add or replace an output. Re-registering an existing id (e.g. a dongle
    /// reconnecting) overwrites the previous entry.
    pub fn register(&self, output: Output) {
        self.outputs
            .lock()
            .expect("registry mutex poisoned")
            .insert(output.id.clone(), output);
    }

    /// Remove an output entirely (e.g. a dongle the user deletes). Disconnects
    /// that should preserve zone membership/name should instead mark the output
    /// offline via [`set_online`](Self::set_online).
    pub fn remove(&self, id: &str) {
        self.outputs
            .lock()
            .expect("registry mutex poisoned")
            .remove(id);
    }

    /// Flip an output's reachability. No-op if the id is unknown.
    pub fn set_online(&self, id: &str, online: bool) {
        if let Some(output) = self
            .outputs
            .lock()
            .expect("registry mutex poisoned")
            .get_mut(id)
        {
            output.online = online;
        }
    }

    /// Resolve an output id to its sink, but only if the output exists *and* is
    /// online. Returns `None` otherwise so callers transparently skip
    /// unreachable outputs.
    pub fn sink(&self, id: &str) -> Option<Arc<dyn AudioSink>> {
        let outputs = self.outputs.lock().expect("registry mutex poisoned");
        outputs
            .get(id)
            .filter(|o| o.online)
            .and_then(|o| o.sink.clone())
    }

    /// Whether an output with this id is currently registered (online or not).
    pub fn contains(&self, id: &str) -> bool {
        self.outputs
            .lock()
            .expect("registry mutex poisoned")
            .contains_key(id)
    }

    /// Snapshot of `(id, name, online)` for every registered output. Used by the
    /// connection layer to report available outputs to the client.
    pub fn list(&self) -> Vec<(OutputId, String, bool)> {
        self.outputs
            .lock()
            .expect("registry mutex poisoned")
            .values()
            .map(|o| (o.id.clone(), o.name.clone(), o.online))
            .collect()
    }
}

impl Default for OutputRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that discards samples — lets us exercise the registry without
    /// opening an audio device.
    struct NullSink;
    impl AudioSink for NullSink {
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            2
        }
        fn write(&self, _samples: &[f32]) {}
    }

    fn output(id: &str, online: bool) -> Output {
        Output {
            id: id.to_string(),
            name: id.to_string(),
            sink: Some(Arc::new(NullSink)),
            online,
        }
    }

    #[test]
    fn sink_resolves_only_online_outputs() {
        let registry = OutputRegistry::new();
        registry.register(output("local", true));
        registry.register(output("kitchen", false));

        assert!(registry.sink("local").is_some());
        // Offline output is registered but not resolvable for playback.
        assert!(registry.sink("kitchen").is_none());
        assert!(registry.contains("kitchen"));
        // Unknown id resolves to nothing.
        assert!(registry.sink("bedroom").is_none());
    }

    #[test]
    fn set_online_toggles_resolution() {
        let registry = OutputRegistry::new();
        registry.register(output("kitchen", false));
        assert!(registry.sink("kitchen").is_none());

        registry.set_online("kitchen", true);
        assert!(registry.sink("kitchen").is_some());

        registry.set_online("kitchen", false);
        assert!(registry.sink("kitchen").is_none());
    }

    #[test]
    fn output_without_sink_never_resolves() {
        let registry = OutputRegistry::new();
        registry.register(Output {
            id: "dongle-1".to_string(),
            name: "Kitchen".to_string(),
            sink: None,
            online: true,
        });
        // Registered + online, but no direct sink: dongles are grouped in
        // snapserver, not decoded into individually.
        assert!(registry.sink("dongle-1").is_none());
        assert!(registry.contains("dongle-1"));
    }

    #[test]
    fn register_overwrites_and_remove_deletes() {
        let registry = OutputRegistry::new();
        registry.register(output("kitchen", false));
        // Re-register the same id online — should overwrite.
        registry.register(output("kitchen", true));
        assert!(registry.sink("kitchen").is_some());

        registry.remove("kitchen");
        assert!(!registry.contains("kitchen"));
        assert!(registry.sink("kitchen").is_none());
    }
}
