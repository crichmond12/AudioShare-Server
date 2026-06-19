//! Playback engine (KAN-20).
//!
//! [`Player`] is the single owner of the audio output and the one in-flight
//! decode pipeline. It is the boundary `commands::dispatch` calls into: `play`
//! starts streaming a URL to the speaker, `stop` halts it.
//!
//! There is at most one stream playing at a time (single-zone for phase 1;
//! independent multi-room is KAN-22). Starting a new stream stops the previous
//! one. The decode work runs on a dedicated OS thread (see [`crate::audio::decode`])
//! and is stopped cooperatively via an `AtomicBool`.
//!
//! A process-wide [`PLAYER`] instance is exposed so the connection handler can
//! reach it without threading a handle through every `Connection`, mirroring the
//! `MAIN_SERVER` global in `server::server`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use lazy_static::lazy_static;

use crate::audio::decode;
use crate::audio::output::AudioOutput;
use crate::audio::sink::AudioSink;

lazy_static! {
    /// Process-wide playback engine used by `commands::dispatch`.
    pub static ref PLAYER: Player = Player::new();
}

/// A running decode pipeline: its cooperative stop flag and the thread driving it.
struct Pipeline {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Pipeline {
    /// Signal the decode thread to stop and wait for it to exit.
    fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

struct PlayerInner {
    // The output device is opened lazily on first successful `play` and kept open
    // afterwards so repeated play/stop don't re-acquire the device.
    output: Option<Arc<dyn AudioSink>>,
    current: Option<Pipeline>,
}

/// The playback engine. Cheap to clone-share via the global [`PLAYER`].
pub struct Player {
    inner: Arc<Mutex<PlayerInner>>,
}

impl Player {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PlayerInner {
                output: None,
                current: None,
            })),
        }
    }

    /// Start streaming `url` to the output device, replacing any current
    /// playback. Returns an error if the audio device can't be opened (this node
    /// can't play audio) — actual stream/decode failures surface later on the
    /// decode thread and simply end playback.
    pub fn play(&self, url: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("player mutex poisoned");

        // Stop any existing playback before starting the new stream.
        if let Some(pipeline) = inner.current.take() {
            pipeline.shutdown();
        }

        // Open the output device lazily, reusing it across plays.
        if inner.output.is_none() {
            let output: Arc<dyn AudioSink> = Arc::new(AudioOutput::new()?);
            inner.output = Some(output);
        }
        let output = Arc::clone(inner.output.as_ref().expect("output set above"));

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_url = url.to_string();

        let handle = thread::Builder::new()
            .name("player".to_string())
            .spawn(move || {
                if let Err(e) = decode::stream_url_to_output(&thread_url, &*output, &thread_stop) {
                    eprintln!("playback ended: {e}");
                }
            })
            .map_err(|e| format!("failed to spawn player thread: {e}"))?;

        inner.current = Some(Pipeline { stop, handle });
        Ok(())
    }

    /// Stop any current playback. No-op if nothing is playing.
    pub fn stop(&self) {
        let mut inner = self.inner.lock().expect("player mutex poisoned");
        if let Some(pipeline) = inner.current.take() {
            pipeline.shutdown();
        }
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}
