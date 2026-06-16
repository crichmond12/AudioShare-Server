//! Thin audio-output layer (KAN-18).
//!
//! `AudioOutput` is the boundary the playback engine (KAN-20) writes PCM into:
//! the engine produces interleaved `f32` samples and calls [`AudioOutput::write`];
//! a cpal output stream drains them to the host's default output device
//! (CoreAudio on macOS for dev, ALSA on the Pi).
//!
//! cpal's `Stream` is `!Send` on some platforms (notably macOS), so it cannot be
//! held in a struct that moves across threads or lives in an async task. We
//! therefore own the stream on a dedicated audio thread and communicate with it
//! only through a shared sample buffer, whose handle *is* `Send + Sync`.
//!
//! The shared buffer is a plain `Mutex<VecDeque<f32>>`. That is deliberately
//! simple: real jitter/underrun buffering and a lock-free hand-off belong to
//! KAN-23 ("Network audio buffering / jitter handling"), which will replace the
//! buffer behind this same `write` interface.

#![allow(dead_code)] // Wired into the playback engine in KAN-20.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Upper bound on buffered samples, so a producer that outruns the output device
/// cannot grow memory without limit. When exceeded, the oldest samples are
/// dropped (bounding latency). Proper bounded buffering is KAN-23's job; this is
/// just a safety valve. ~4s of stereo audio at 48 kHz.
const MAX_BUFFERED_SAMPLES: usize = 48_000 * 2 * 4;

type SampleBuffer = Arc<Mutex<VecDeque<f32>>>;

/// A handle to a running audio output. Dropping it stops the stream and joins
/// the audio thread.
pub struct AudioOutput {
    buffer: SampleBuffer,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    sample_rate: u32,
    channels: u16,
}

impl AudioOutput {
    /// Open the default output device and start its stream. Returns an error if
    /// no output device is available or the stream cannot be built — callers
    /// (the engine) should treat this as "this node can't play audio" rather
    /// than panicking.
    pub fn new() -> Result<Self, String> {
        let buffer: SampleBuffer = Arc::new(Mutex::new(VecDeque::new()));
        let running = Arc::new(AtomicBool::new(true));

        let (init_tx, init_rx) = mpsc::channel::<Result<(u32, u16), String>>();
        let thread_buffer = Arc::clone(&buffer);
        let thread_running = Arc::clone(&running);

        let handle = thread::Builder::new()
            .name("audio-output".to_string())
            .spawn(move || run_audio_thread(thread_buffer, thread_running, init_tx))
            .map_err(|e| format!("failed to spawn audio thread: {e}"))?;

        // Wait for the thread to report whether the device opened successfully.
        let (sample_rate, channels) = match init_rx.recv() {
            Ok(Ok(meta)) => meta,
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(e);
            }
            Err(_) => {
                let _ = handle.join();
                return Err("audio thread exited before initialization".to_string());
            }
        };

        Ok(Self {
            buffer,
            running,
            handle: Some(handle),
            sample_rate,
            channels,
        })
    }

    /// Push interleaved `f32` PCM frames to the output. Samples must already be
    /// at [`sample_rate`](Self::sample_rate) / [`channels`](Self::channels);
    /// resampling/format conversion is the engine's responsibility (KAN-20/21).
    pub fn write(&self, samples: &[f32]) {
        let mut buf = self.buffer.lock().expect("audio buffer poisoned");
        buf.extend(samples.iter().copied());
        if buf.len() > MAX_BUFFERED_SAMPLES {
            let overflow = buf.len() - MAX_BUFFERED_SAMPLES;
            buf.drain(..overflow);
        }
    }

    /// The output device's sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The output device's channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Body of the dedicated audio thread: open the device, build and play the
/// stream, report init status, then keep the stream alive until shutdown.
fn run_audio_thread(
    buffer: SampleBuffer,
    running: Arc<AtomicBool>,
    init_tx: mpsc::Sender<Result<(u32, u16), String>>,
) {
    let opened = (|| -> Result<(cpal::Stream, u32, u16), String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("no default output config: {e}"))?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let sample_rate = config.sample_rate.0;
        let channels = config.channels;

        // cpal hands the device callback whatever sample type the device uses;
        // we convert our f32 buffer into it. f32 is the common case on both
        // CoreAudio and ALSA.
        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, Arc::clone(&buffer)),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, Arc::clone(&buffer)),
            SampleFormat::U16 => build_stream::<u16>(&device, &config, Arc::clone(&buffer)),
            other => Err(format!("unsupported sample format: {other:?}")),
        }?;
        stream
            .play()
            .map_err(|e| format!("failed to start stream: {e}"))?;
        Ok((stream, sample_rate, channels))
    })();

    match opened {
        Ok((stream, sample_rate, channels)) => {
            // Report success, then hold the stream alive on this thread until
            // the owning AudioOutput is dropped.
            let _ = init_tx.send(Ok((sample_rate, channels)));
            while running.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
            drop(stream);
        }
        Err(e) => {
            let _ = init_tx.send(Err(e));
        }
    }
}

/// Build an output stream whose callback drains the shared buffer into the
/// device, emitting silence on underrun.
fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: SampleBuffer,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut buf = buffer.lock().expect("audio buffer poisoned");
                fill_output(&mut buf, data);
            },
            move |err| eprintln!("audio output stream error: {err}"),
            None,
        )
        .map_err(|e| format!("failed to build output stream: {e}"))
}

/// Copy as many samples as available from `buffer` into `output`, converting to
/// the device sample type, and fill any remainder with silence (underrun).
/// Pulled out as a free function so the drain/underrun behavior is unit-testable
/// without opening an audio device.
fn fill_output<T: Sample + FromSample<f32>>(buffer: &mut VecDeque<f32>, output: &mut [T]) {
    for slot in output.iter_mut() {
        let sample = buffer.pop_front().unwrap_or(0.0);
        *slot = T::from_sample(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_output_drains_then_pads_with_silence() {
        let mut buffer: VecDeque<f32> = VecDeque::from(vec![0.5, -0.5, 1.0]);
        let mut output = [9.0f32; 5];

        fill_output(&mut buffer, &mut output);

        // First three are drained in order; the rest are silence (0.0).
        assert_eq!(output, [0.5, -0.5, 1.0, 0.0, 0.0]);
        // Buffer is fully consumed.
        assert!(buffer.is_empty());
    }

    #[test]
    fn fill_output_leaves_remaining_samples_for_next_callback() {
        let mut buffer: VecDeque<f32> = VecDeque::from(vec![1.0, 2.0, 3.0, 4.0]);
        let mut output = [0.0f32; 2];

        fill_output(&mut buffer, &mut output);

        assert_eq!(output, [1.0, 2.0]);
        // Unconsumed samples stay queued in order.
        assert_eq!(buffer, VecDeque::from(vec![3.0, 4.0]));
    }

    // Opening a real device requires audio hardware, so this is opt-in: run with
    // `cargo test -- --ignored` on a machine with an output device.
    #[test]
    #[ignore]
    fn opens_default_output_device() {
        let output = AudioOutput::new().expect("should open default output device");
        assert!(output.sample_rate() > 0);
        assert!(output.channels() > 0);
        output.write(&[0.0; 256]);
    }
}
