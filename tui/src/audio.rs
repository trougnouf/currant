// ./tui/src/audio.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The audio backend for the TUI. Owns the rodio output stream and drives
//! playback from the controller's live queue. Opus falls back to the bundled
//! libopus decoder; everything else is decoded by symphonia via rodio.

use cassis_core::controller::PlayerController;
use cassis_core::model::PlayerIntent;
use rodio::{Decoder, DeviceSinkBuilder, Player, Source};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Shared playback position + seek channel between the audio thread and the
/// UI. Position is in milliseconds; the audio thread writes it every loop
/// iteration, the UI reads it each frame. Seek requests use a sentinel of -1
/// (no pending seek) or a non-negative millisecond target.
pub struct PlaybackState {
    position_ms: AtomicU64,
    seek_ms: AtomicI64,
}

impl PlaybackState {
    pub fn new() -> Self {
        Self {
            position_ms: AtomicU64::new(0),
            seek_ms: AtomicI64::new(-1),
        }
    }

    pub fn position_ms(&self) -> u64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    pub fn request_seek(&self, ms: u64) {
        self.seek_ms.store(ms as i64, Ordering::Relaxed);
    }

    fn take_seek(&self) -> Option<u64> {
        let v = self.seek_ms.swap(-1, Ordering::Relaxed);
        if v >= 0 { Some(v as u64) } else { None }
    }

    fn set_position(&self, ms: u64) {
        self.position_ms.store(ms, Ordering::Relaxed);
    }
}

/// Minimum fraction/length of a track before a completion scrobble is sent.
fn scrobble_threshold(duration_secs: u32) -> Duration {
    if duration_secs == 0 {
        Duration::from_secs(30)
    } else {
        Duration::from_secs((duration_secs as u64 / 2).min(240))
    }
}

/// Open `path` as a rodio `Source`. Tries symphonia first, then the opus
/// decoder when the `opus` feature is enabled.
fn open_source(path: &str) -> Option<Box<dyn Source<Item = f32> + Send>> {
    let p = Path::new(path);
    if let Ok(file) = File::open(p)
        && let Ok(decoder) = Decoder::try_from(file)
    {
        return Some(Box::new(decoder));
    }
    #[cfg(feature = "opus")]
    {
        let is_opus = p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("opus") || e.eq_ignore_ascii_case("ogg"));
        if is_opus && let Ok(src) = crate::opus::OpusSource::open(p) {
            return Some(Box::new(src));
        }
    }
    #[cfg(not(feature = "opus"))]
    {
        let _ = p;
    }
    None
}

/// Spawn the audio thread. It owns the output stream and polls the controller.
pub fn spawn(
    controller: Arc<Mutex<PlayerController>>,
    state: Arc<PlaybackState>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let stream = match DeviceSinkBuilder::open_default_sink() {
            Ok(s) => s,
            Err(_) => return,
        };
        let player = Player::connect_new(stream.mixer());

        let mut playing_id: Option<String> = None;
        let mut started_at = Instant::now();
        let mut threshold = Duration::from_secs(30);

        loop {
            let (is_playing, current, volume) = {
                let c = controller.lock().unwrap();
                (c.is_playing, c.current_track.clone(), c.volume)
            };

            player.set_volume(volume);

            // Publish the current playback position for the UI.
            state.set_position(player.get_pos().as_millis() as u64);

            // Process pending seek requests.
            if let Some(target_ms) = state.take_seek()
                && playing_id.is_some()
            {
                let _ = player.try_seek(Duration::from_millis(target_ms));
                state.set_position(target_ms);
            }

            if !is_playing {
                player.pause();
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            player.play();

            // When no track is current but we should be playing, pull the
            // next track from the queue (explicit or dynamic).
            let current = if current.is_none() {
                controller.lock().unwrap().determine_next_track()
            } else {
                current
            };

            // A new track was chosen (play/skip/previous).
            if current != playing_id {
                if let Some(prev) = playing_id.take() {
                    if started_at.elapsed() >= threshold {
                        controller.lock().unwrap().on_track_completed(&prev);
                    }
                    player.skip_one();
                    thread::sleep(Duration::from_millis(10));
                }

                if let Some(id) = &current {
                    let path = controller.lock().unwrap().store.get_path(id);
                    match path.as_deref().and_then(open_source) {
                        Some(src) => {
                            let dur = controller
                                .lock()
                                .unwrap()
                                .store
                                .get_track(id)
                                .map(|t| t.duration_secs)
                                .unwrap_or(0);
                            threshold = scrobble_threshold(dur);
                            player.append(src);
                            player.play();
                            playing_id = Some(id.clone());
                            started_at = Instant::now();
                            controller.lock().unwrap().on_track_started(id);
                        }
                        None => {
                            controller.lock().unwrap().dispatch(PlayerIntent::NextTrack);
                        }
                    }
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Same track: detect natural end.
            if playing_id.is_some() && player.empty() {
                let prev = playing_id.take().unwrap();
                if started_at.elapsed() >= threshold {
                    controller.lock().unwrap().on_track_completed(&prev);
                }
                controller.lock().unwrap().dispatch(PlayerIntent::NextTrack);
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            thread::sleep(Duration::from_millis(80));
        }
    })
}
