// ./tui/src/audio.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The audio backend for the TUI. Owns the rodio output stream and drives
//! playback from the controller's live queue. Opus falls back to the bundled
//! libopus decoder; everything else is decoded by symphonia via rodio.

use cassis_core::controller::PlayerController;
use cassis_core::model::PlayerIntent;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Minimum fraction/length of a track before a completion scrobble is sent.
fn scrobble_threshold(duration_secs: u32) -> Duration {
    if duration_secs == 0 {
        Duration::from_secs(30)
    } else {
        Duration::from_secs((duration_secs as u64 / 2).min(240))
    }
}

/// Open `path` as a rodio `Source`. Tries symphonia first, then the opus
/// decoder when the `opus` feature is enabled. Sink::append already calls
/// convert_samples() internally, so we return the raw decoder.
fn open_source(path: &str) -> Option<Box<dyn Source<Item = f32> + Send>> {
    let p = Path::new(path);
    if let Ok(file) = File::open(p)
        && let Ok(decoder) = Decoder::new(BufReader::new(file))
    {
        return Some(Box::new(decoder.convert_samples()));
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
pub fn spawn(controller: Arc<Mutex<PlayerController>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let (_stream, handle) = match OutputStream::try_default() {
            Ok(s) => s,
            Err(_) => return,
        };
        let sink = match Sink::try_new(&handle) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut playing_id: Option<String> = None;
        let mut started_at = Instant::now();
        let mut threshold = Duration::from_secs(30);

        loop {
            let (is_playing, current, volume) = {
                let c = controller.lock().unwrap();
                (c.is_playing, c.current_track.clone(), c.volume)
            };

            sink.set_volume(volume);

            if !is_playing {
                sink.pause();
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            sink.play();

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
                    // Skip the old source non-blockingly. skip_one() sets
                    // a flag that the periodic access checks every 5ms to
                    // advance to the next source. clear() would block on
                    // sleep_until_end(); stop() would also block in append().
                    sink.skip_one();
                    // Give the skip flag time to propagate before appending.
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
                            sink.append(src);
                            sink.play();
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
            if playing_id.is_some() && sink.empty() {
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
