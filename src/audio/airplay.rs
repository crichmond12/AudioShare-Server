//! AirPlay receive path (Phase 4, Slice 1).
//!
//! The hub's first **receiver** source: audio is pushed *to* us by the phone's
//! own app, rather than fetched by the hub. A supervised `shairport-sync`
//! (classic AirPlay) writes raw `s16le` 44100/2 PCM into a named FIFO via its
//! `pipe` backend; [`pump_fifo_to_sink`] reads that FIFO, converts to `f32`,
//! resamples/mixes to the sink's format (reusing [`crate::audio::decode`]'s
//! pipeline), and writes into an [`AudioSink`]. Snapcast stays untouched — an
//! AirPlay source resolves to a zone's sink through the same seam a URL does.
//!
//! This mirrors `audio::snapcast` in reverse. Slice 1 proves the path with a
//! demo-gated test; per-zone supervision + engine wiring is Slice 2.

/// AirPlay always delivers CD audio: 44.1 kHz, 16-bit, stereo.
pub const AIRPLAY_SAMPLE_RATE: u32 = 44_100;
pub const AIRPLAY_CHANNELS: usize = 2;

/// Convert interleaved little-endian `s16` `bytes` into `channels` planar `f32`
/// channels in `[-1.0, 1.0]`. A trailing partial frame (fewer than
/// `channels * 2` bytes) is ignored — callers carry the remainder.
pub(crate) fn i16le_to_planar_f32(bytes: &[u8], channels: usize) -> Vec<Vec<f32>> {
    let frame_bytes = channels * 2;
    let frames = if frame_bytes == 0 { 0 } else { bytes.len() / frame_bytes };
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for f in 0..frames {
        for (ch, plane) in planar.iter_mut().enumerate() {
            let i = (f * channels + ch) * 2;
            let sample = i16::from_le_bytes([bytes[i], bytes[i + 1]]);
            plane.push(sample as f32 / i16::MAX as f32);
        }
    }
    planar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16le_to_planar_deinterleaves_and_scales() {
        // Two stereo frames: (L=i16::MAX, R=0), (L=-i16::MAX, R=i16::MIN+1).
        // i16::MAX -> ~1.0, 0 -> 0.0, -i16::MAX -> ~-1.0.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&i16::MAX.to_le_bytes()); // L0
        bytes.extend_from_slice(&0i16.to_le_bytes()); // R0
        bytes.extend_from_slice(&(-i16::MAX).to_le_bytes()); // L1
        bytes.extend_from_slice(&(-i16::MAX).to_le_bytes()); // R1

        let planar = i16le_to_planar_f32(&bytes, 2);
        assert_eq!(planar.len(), 2);
        assert_eq!(planar[0].len(), 2); // 2 frames per channel
        assert!((planar[0][0] - 1.0).abs() < 1e-3); // L0 ~ +1.0
        assert!(planar[1][0].abs() < 1e-6); // R0 == 0.0
        assert!((planar[0][1] + 1.0).abs() < 1e-3); // L1 ~ -1.0
    }

    #[test]
    fn i16le_to_planar_ignores_trailing_partial_frame() {
        // 5 bytes = one whole stereo frame (4 bytes) + 1 stray byte.
        let bytes = [0u8, 0, 0, 0, 0];
        let planar = i16le_to_planar_f32(&bytes, 2);
        assert_eq!(planar[0].len(), 1);
        assert_eq!(planar[1].len(), 1);
    }
}
