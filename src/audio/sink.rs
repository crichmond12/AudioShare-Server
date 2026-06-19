//! The `AudioSink` boundary (multi-room refactor, Change 1).
//!
//! This trait is the keystone of multi-room: the decode pipeline
//! ([`crate::audio::decode`]) writes interleaved `f32` PCM into *some* sink
//! without knowing whether it is the local cpal device ([`AudioOutput`]) or,
//! later, a network sink streaming to a remote receiver ("dongle"). Decoupling
//! decode from the concrete output is what lets one engine drive many outputs.
//!
//! Implementors accept samples already at their own
//! [`sample_rate`](AudioSink::sample_rate) / [`channels`](AudioSink::channels);
//! resampling and channel mixing to that format are the caller's responsibility
//! (the decode pipeline already does this against these same two methods).

use crate::audio::output::AudioOutput;

/// A destination for interleaved `f32` PCM frames.
pub trait AudioSink: Send + Sync {
    /// The sink's expected sample rate in Hz.
    fn sample_rate(&self) -> u32;
    /// The sink's expected channel count.
    fn channels(&self) -> u16;
    /// Push interleaved `f32` PCM frames to the sink. Samples must already be at
    /// this sink's `sample_rate` / `channels`.
    fn write(&self, samples: &[f32]);
}

// The local cpal device is just one kind of sink. These forward to the inherent
// methods on `AudioOutput`, which already have exactly this shape.
impl AudioSink for AudioOutput {
    fn sample_rate(&self) -> u32 {
        AudioOutput::sample_rate(self)
    }
    fn channels(&self) -> u16 {
        AudioOutput::channels(self)
    }
    fn write(&self, samples: &[f32]) {
        AudioOutput::write(self, samples)
    }
}
