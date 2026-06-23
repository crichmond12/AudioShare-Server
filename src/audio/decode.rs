//! Internet-radio decode pipeline (KAN-20/21).
//!
//! [`stream_url_to_output`] is the body of the player's decode thread: it pulls a
//! DRM-free HTTP audio stream (MP3/AAC internet radio), decodes it with
//! Symphonia, resamples to the output device's rate with Rubato, mixes to the
//! device's channel count, and feeds interleaved `f32` PCM to [`AudioOutput`].
//!
//! It runs on a dedicated OS thread (Symphonia and `reqwest::blocking` are both
//! synchronous) and stops cooperatively when `stop` is set.
//!
//! Pacing is intentionally minimal: a live radio server emits bytes at roughly
//! playback rate, so the blocking network read paces the loop. For a fast
//! (non-live) source the decode can outrun the device; [`AudioOutput::write`]
//! bounds growth by dropping the oldest samples. Proper jitter/buffer handling
//! is KAN-23.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;

use crate::audio::sink::AudioSink;

/// Number of input frames fed to the resampler per chunk. A small power of two
/// keeps latency low while amortizing per-call overhead.
const RESAMPLE_CHUNK: usize = 1024;

/// Open `url`, decode it, and stream PCM into `output` until the stream ends, an
/// unrecoverable error occurs, or `stop` is set. Returns `Err` only for failures
/// that prevented playback from starting or continuing meaningfully (bad URL,
/// no audio track, unsupported codec); a normal end-of-stream returns `Ok`.
pub fn stream_url_to_output(
    url: &str,
    output: &dyn AudioSink,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    // Fetch the stream. We deliberately do NOT request ICY metadata
    // (`Icy-MetaData: 1`), so the server returns a pure audio byte stream with no
    // interleaved metadata to strip.
    let response = reqwest::blocking::Client::new()
        .get(url)
        .send()
        .map_err(|e| format!("http request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("http status error: {e}"))?;

    // Wrap the blocking HTTP body (Read) as an unseekable Symphonia source.
    let source = ReadOnlySource::new(response);
    let mss = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());

    // Probe the container/codec and build a format reader.
    let mut format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("could not probe stream format: {e}"))?;

    // Select the default audio track and clone its codec params so the immutable
    // borrow of `format` is released before we start pulling packets.
    let (track_id, audio_params) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or("stream has no audio track")?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .cloned()
            .ok_or("audio track has no codec parameters")?;
        (track.id, params)
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("unsupported audio codec: {e}"))?;

    let out_rate = output.sample_rate();
    let out_channels = output.channels() as usize;

    // The resampler is built lazily once the source's rate/channels are known
    // from the first decoded buffer, and rebuilt if the stream's spec changes.
    let mut pipeline: Option<ResamplePipeline> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => return Ok(()), // clean end of stream
            // Symphonia surfaces end-of-stream on an unseekable source as an
            // unexpected-EOF IoError; treat it as a normal end.
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            Err(e) => return Err(format!("error reading stream: {e}")),
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A single malformed packet is recoverable: skip it and continue.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                pipeline = None;
                continue;
            }
            Err(e) => return Err(format!("decode error: {e}")),
        };

        if decoded.frames() == 0 {
            continue;
        }

        let spec = decoded.spec();
        let in_rate = spec.rate();
        let in_channels = spec.channels().count();

        // Decode this buffer to planar f32, then mix to the device channel count.
        let mut planar: Vec<Vec<f32>> = Vec::new();
        decoded.copy_to_vecs_planar::<f32>(&mut planar);
        let mixed = mix_planar(&planar, out_channels);

        // (Re)build the resample pipeline if the source spec changed.
        let needs_rebuild = match &pipeline {
            Some(p) => p.in_rate != in_rate || p.in_channels != in_channels,
            None => true,
        };
        if needs_rebuild {
            pipeline = Some(ResamplePipeline::new(
                in_rate,
                in_channels,
                out_rate,
                out_channels,
            )?);
        }
        let pipeline = pipeline.as_mut().expect("pipeline built above");

        pipeline.push_and_drain(mixed, output);
    }
}

/// Holds the resampler (or a passthrough when rates match) plus the per-channel
/// accumulator that feeds the resampler its required fixed-size chunks.
pub(crate) struct ResamplePipeline {
    in_rate: u32,
    in_channels: usize,
    out_channels: usize,
    // `None` => input and output rates match, so no resampling is needed.
    resampler: Option<FastFixedIn<f32>>,
    // Pending mixed (output-channel) samples awaiting a full resample chunk.
    accum: Vec<Vec<f32>>,
}

impl ResamplePipeline {
    pub(crate) fn new(
        in_rate: u32,
        in_channels: usize,
        out_rate: u32,
        out_channels: usize,
    ) -> Result<Self, String> {
        let resampler = if in_rate == out_rate {
            None
        } else {
            Some(
                FastFixedIn::<f32>::new(
                    out_rate as f64 / in_rate as f64,
                    1.0, // fixed ratio; we never retune mid-stream
                    PolynomialDegree::Cubic,
                    RESAMPLE_CHUNK,
                    out_channels,
                )
                .map_err(|e| format!("failed to build resampler: {e}"))?,
            )
        };
        Ok(Self {
            in_rate,
            in_channels,
            out_channels,
            resampler,
            accum: vec![Vec::new(); out_channels],
        })
    }

    /// Append a mixed (output-channel, source-rate) planar buffer, then emit as
    /// much resampled, interleaved audio to `output` as is now available.
    pub(crate) fn push_and_drain(&mut self, mixed: Vec<Vec<f32>>, output: &dyn AudioSink) {
        match self.resampler.as_mut() {
            // No resampling needed: interleave and write directly.
            None => output.write(&interleave(&mixed)),
            Some(resampler) => {
                for ch in 0..self.out_channels {
                    self.accum[ch].extend_from_slice(&mixed[ch]);
                }
                while self.accum[0].len() >= RESAMPLE_CHUNK {
                    let chunk: Vec<Vec<f32>> = self
                        .accum
                        .iter()
                        .map(|c| c[..RESAMPLE_CHUNK].to_vec())
                        .collect();
                    for c in self.accum.iter_mut() {
                        c.drain(..RESAMPLE_CHUNK);
                    }
                    match resampler.process(&chunk, None) {
                        Ok(resampled) => output.write(&interleave(&resampled)),
                        Err(e) => eprintln!("resample error: {e}"),
                    }
                }
            }
        }
    }
}

/// Mix `input` planar channels to exactly `out_channels` planar channels of the
/// same frame length. Handles the common internet-radio cases (mono→stereo
/// duplicate, stereo→mono average, equal pass-through); for other combinations
/// each output channel maps to source channel `i % in_channels`.
pub(crate) fn mix_planar(input: &[Vec<f32>], out_channels: usize) -> Vec<Vec<f32>> {
    let in_channels = input.len();
    if in_channels == 0 || out_channels == 0 {
        return vec![Vec::new(); out_channels];
    }
    let frames = input[0].len();

    if in_channels == out_channels {
        return input.to_vec();
    }
    if in_channels == 1 {
        // Duplicate the single channel into every output channel.
        return vec![input[0].clone(); out_channels];
    }
    if out_channels == 1 {
        // Average all source channels into one.
        let mut mono = vec![0.0f32; frames];
        for ch in input {
            for (m, s) in mono.iter_mut().zip(ch.iter()) {
                *m += *s;
            }
        }
        let scale = 1.0 / in_channels as f32;
        for m in mono.iter_mut() {
            *m *= scale;
        }
        return vec![mono];
    }
    // General fallback: round-robin source channels across the outputs.
    (0..out_channels)
        .map(|i| input[i % in_channels].clone())
        .collect()
}

/// Interleave planar channel buffers into a single interleaved `f32` buffer in
/// canonical channel order. Channels are assumed equal length (shortest wins).
fn interleave(planar: &[Vec<f32>]) -> Vec<f32> {
    let channels = planar.len();
    if channels == 0 {
        return Vec::new();
    }
    let frames = planar.iter().map(|c| c.len()).min().unwrap_or(0);
    let mut out = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        for ch in planar {
            out.push(ch[frame]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_orders_samples_by_frame() {
        let planar = vec![vec![1.0, 2.0, 3.0], vec![10.0, 20.0, 30.0]];
        assert_eq!(interleave(&planar), vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);
    }

    #[test]
    fn interleave_uses_shortest_channel() {
        let planar = vec![vec![1.0, 2.0, 3.0], vec![10.0, 20.0]];
        assert_eq!(interleave(&planar), vec![1.0, 10.0, 2.0, 20.0]);
    }

    #[test]
    fn mix_equal_channels_passes_through() {
        let input = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        assert_eq!(mix_planar(&input, 2), input);
    }

    #[test]
    fn mix_mono_to_stereo_duplicates() {
        let input = vec![vec![1.0, 2.0, 3.0]];
        let out = mix_planar(&input, 2);
        assert_eq!(out, vec![vec![1.0, 2.0, 3.0], vec![1.0, 2.0, 3.0]]);
    }

    #[test]
    fn mix_stereo_to_mono_averages() {
        let input = vec![vec![1.0, 3.0], vec![3.0, 5.0]];
        let out = mix_planar(&input, 1);
        assert_eq!(out, vec![vec![2.0, 4.0]]);
    }

    // Live end-to-end smoke test: fetch a real internet-radio stream, decode it,
    // and play ~3s to the default output device. Requires network + audio
    // hardware, so it is opt-in:
    //   cargo test audio::decode::tests::plays_internet_radio_briefly -- --ignored --nocapture
    // You should hear audio. The test only asserts the pipeline didn't error
    // before the stop signal.
    #[test]
    #[ignore]
    fn plays_internet_radio_briefly() {
        use crate::audio::output::AudioOutput;
        use std::thread;
        use std::time::Duration;

        const URL: &str = "https://ice1.somafm.com/groovesalad-128-mp3";

        let output = AudioOutput::new().expect("open default output device");
        let stop = Arc::new(AtomicBool::new(false));

        let stop_for_thread = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            // Sequential here for simplicity: stop is flipped from the main
            // thread below while this runs.
            super::stream_url_to_output(URL, &output, &stop_for_thread)
        });

        thread::sleep(Duration::from_secs(3));
        stop.store(true, Ordering::Relaxed);

        let result = worker.join().expect("player thread panicked");
        assert!(result.is_ok(), "playback errored: {result:?}");
    }
}
