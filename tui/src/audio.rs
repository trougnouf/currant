// ./tui/src/audio.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The audio backend for the TUI. Owns the rodio output stream and drives
//! playback from the controller's live queue. Opus falls back to the bundled
//! libopus decoder; everything else is decoded by symphonia via rodio.

use currant_core::controller::{ErrorBus, PlaybackState, PlayerController};
use currant_core::model::{PlayerIntent, Track};
use currant_core::net::NetworkState;
use currant_core::net::stream::{HttpSeekableReader, Seekable};
use currant_core::store::LibraryStore;
use lofty::file::TaggedFileExt;
use rodio::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rodio::{Decoder, Player, Source};
use std::fs::File;
use std::num::NonZero;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How far into a track the user must have listened before a completion
/// scrobble is sent: half its duration, capped at 4 minutes. Tracks shorter
/// than 30 seconds are never scrobbled; an unknown duration falls back to
/// 30 seconds of listening.
fn scrobble_threshold(duration_secs: u32) -> Option<Duration> {
    if duration_secs < 30 {
        return (duration_secs == 0).then(|| Duration::from_secs(30));
    }
    Some(Duration::from_secs((duration_secs as u64 / 2).min(240)))
}

/// The ReplayGain multiplier for a track, read from its
/// `REPLAYGAIN_TRACK_GAIN` tag. Returns 1.0 when the tag is missing or
/// unparseable.
fn track_gain_multiplier(path: &Path) -> f32 {
    let tagged_file = match lofty::probe::Probe::open(path)
        .ok()
        .and_then(|p| p.read().ok())
    {
        Some(f) => f,
        None => return 1.0,
    };
    let tag = match tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    {
        Some(t) => t,
        None => return 1.0,
    };
    let db = match tag.get_string(lofty::tag::ItemKey::ReplayGainTrackGain) {
        Some(s) => match s.replace(" dB", "").replace("dB", "").trim().parse::<f32>() {
            Ok(v) => v,
            Err(_) => return 1.0,
        },
        None => return 1.0,
    };
    10.0_f32.powf(db / 20.0).clamp(0.1, 10.0)
}

/// A lazily-created seekable reader for one playback source.
type ReaderFactory = Box<dyn Fn() -> Option<Box<dyn Seekable>> + Send>;

/// Open a track as a rodio `Source`. Sources are tried best first: the local
/// file when we hold one, then connected peers holding a copy in
/// format-tier order (see `LibraryStore::get_remote_source_peers`). Each
/// source is tried with symphonia and, for Opus/OGG files, the bundled opus
/// decoder, before moving on to the next source.
fn open_source(
    track: &Track,
    store: &LibraryStore,
    network: &Arc<Mutex<NetworkState>>,
    bus: &ErrorBus,
) -> Option<Box<dyn Source<Item = f32> + Send>> {
    let mut candidates: Vec<(String, ReaderFactory)> = Vec::new();

    if track.is_local {
        let path = track.path.clone();
        candidates.push((
            path.clone(),
            Box::new(move || {
                File::open(&path)
                    .ok()
                    .map(|f| Box::new(f) as Box<dyn Seekable>)
            }),
        ));
    }

    let token = store.load_pairing_token();
    for (peer_id, peer_path) in store.get_remote_source_peers(&track.id) {
        let peer = {
            let state = network.lock().unwrap();
            state.peers.get(&peer_id).cloned()
        };
        let Some(peer) = peer else { continue };
        let ips = peer.ips;
        let http_port = peer.http_port;
        let track_id = track.id.clone();
        let token = token.clone();
        candidates.push((
            peer_path,
            Box::new(move || {
                Some(Box::new(
                    HttpSeekableReader::new(&ips, http_port, &track_id, token.clone()).ok()?,
                ))
            }),
        ));
    }

    for (path, make_reader) in &candidates {
        // A fresh seekable reader per attempt: each decoder consumes its
        // reader, so a failed attempt must not leave the next one starting
        // mid-file.
        if let Some(reader) = make_reader()
            && let Ok(decoder) = Decoder::new(reader)
        {
            return Some(Box::new(decoder));
        }
        #[cfg(feature = "opus")]
        {
            let is_opus = Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("opus") || e.eq_ignore_ascii_case("ogg"));
            if is_opus
                && let Some(reader) = make_reader()
                && let Ok(src) = crate::opus::OpusSource::open(reader)
            {
                return Some(Box::new(src));
            }
        }
    }

    bus.push(format!("failed to open track {}: {}", track.id, track.path));
    None
}

/// Build the audio output pipeline: the default output device, a cpal stream
/// fed by a fresh rodio mixer, and a player connected to that mixer. The
/// stream's error callback sets `broken` so the audio loop can rebuild the
/// pipeline when the device fails — e.g. ALSA `POLLERR` when a Bluetooth
/// A2DP sink drops — which cpal does not recover on its own. Returns `None`
/// when no output device is available or the stream cannot be built.
fn build_output(
    host: &rodio::cpal::Host,
    broken: &Arc<AtomicBool>,
    bus: &Arc<ErrorBus>,
) -> Option<(Box<dyn StreamTrait>, Player)> {
    let device = host.default_output_device()?;
    let supported_config = device.default_output_config().ok()?;
    let sample_format = supported_config.sample_format();
    let config: rodio::cpal::StreamConfig = supported_config.into();

    let (mixer, mut mixer_out) = match (
        NonZero::new(config.channels),
        NonZero::new(config.sample_rate),
    ) {
        (Some(channels), Some(sample_rate)) => rodio::mixer::mixer(channels, sample_rate),
        _ => return None,
    };

    let broken_cb = broken.clone();
    let bus_cb = bus.clone();
    let err_fn = move |err| {
        broken_cb.store(true, Ordering::SeqCst);
        bus_cb.push(format!("audio stream error: {err}"));
    };

    // Feed the mixer output into a raw cpal stream so we keep ownership of
    // the stream and can cork it at the OS level while paused.
    let stream = match sample_format {
        rodio::cpal::SampleFormat::F32 => device.build_output_stream(
            &config,
            move |data: &mut [f32], _| {
                for sample in data.iter_mut() {
                    *sample = mixer_out.next().unwrap_or(0.0);
                }
            },
            err_fn,
            None,
        ),
        rodio::cpal::SampleFormat::I16 => device.build_output_stream(
            &config,
            move |data: &mut [i16], _| {
                for sample in data.iter_mut() {
                    *sample = (mixer_out.next().unwrap_or(0.0).clamp(-1.0, 1.0)
                        * f32::from(i16::MAX)) as i16;
                }
            },
            err_fn,
            None,
        ),
        rodio::cpal::SampleFormat::U16 => device.build_output_stream(
            &config,
            move |data: &mut [u16], _| {
                for sample in data.iter_mut() {
                    *sample = ((mixer_out.next().unwrap_or(0.0).clamp(-1.0, 1.0) + 1.0)
                        * 0.5
                        * f32::from(u16::MAX)) as u16;
                }
            },
            err_fn,
            None,
        ),
        rodio::cpal::SampleFormat::I32 => device.build_output_stream(
            &config,
            move |data: &mut [i32], _| {
                for sample in data.iter_mut() {
                    *sample = (mixer_out.next().unwrap_or(0.0).clamp(-1.0, 1.0) * (i32::MAX as f32))
                        as i32;
                }
            },
            err_fn,
            None,
        ),
        rodio::cpal::SampleFormat::F64 => device.build_output_stream(
            &config,
            move |data: &mut [f64], _| {
                for sample in data.iter_mut() {
                    *sample = f64::from(mixer_out.next().unwrap_or(0.0));
                }
            },
            err_fn,
            None,
        ),
        _ => return None, // Unsupported sample format
    }
    .ok()?;

    let stream: Box<dyn StreamTrait> = Box::new(stream);
    let player = Player::connect_new(&mixer);
    Some((stream, player))
}

/// Spawn the audio thread. It owns the output stream and polls the controller.
pub fn spawn(
    controller: Arc<Mutex<PlayerController>>,
    state: Arc<PlaybackState>,
    network: Arc<Mutex<NetworkState>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let host = rodio::cpal::default_host();
        let bus = controller.lock().unwrap().error_bus.clone();

        // The output pipeline (device + cpal stream + mixer + player) is built
        // once and torn down and rebuilt whenever the stream reports an error.
        // cpal does not recover a broken stream on its own — e.g. ALSA
        // POLLERR when a Bluetooth A2DP sink drops — so without this the only
        // remedy is restarting the player.
        let broken = Arc::new(AtomicBool::new(false));
        let mut stream: Option<Box<dyn StreamTrait>> = None;
        let mut player: Option<Player> = None;

        let mut playing_id: Option<String> = None;
        let mut threshold: Option<Duration> = Some(Duration::from_secs(30));
        // A seek that arrived before it could be applied (no track loaded
        // yet, or the stream corked); applied to the track in the player,
        // or remembered for the next one that loads.
        let mut pending_seek: Option<u64> = None;
        let mut rg_key: (Option<String>, bool) = (None, false);
        let mut current_rg_multiplier = 1.0_f32;
        let mut stream_paused = false;

        loop {
            // Build or rebuild the output pipeline. `broken` is set by the
            // stream's error callback; `stream.is_none()` covers the first
            // pass and a failed build we are retrying.
            if stream.is_none() || broken.swap(false, Ordering::SeqCst) {
                let is_rebuild = stream.is_some();
                drop(stream.take());
                drop(player.take());
                match build_output(&host, &broken, &bus) {
                    Some((s, p)) => {
                        let is_playing = controller.lock().unwrap().is_playing;
                        if s.play().is_err() || (!is_playing && s.pause().is_err()) {
                            // The stream could not be started. Drop it and
                            // retry after a pause so a transient device
                            // hiccup — or a Bluetooth speaker that is still
                            // connecting — is picked up without a restart.
                            thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                        if is_rebuild {
                            // Re-open the current source on the fresh player
                            // and resume where the user left off, so a
                            // transient device drop does not restart the
                            // track from the top.
                            if let Some(id) = &playing_id {
                                let store = controller.lock().unwrap().store.clone();
                                if let Some(track) = store.get_track(id)
                                    && let Some(src) = open_source(&track, &store, &network, &bus)
                                {
                                    let pos = state.position_ms();
                                    p.append(src);
                                    if pos > 0 {
                                        let _ = p.try_seek(Duration::from_millis(pos));
                                    }
                                    if is_playing {
                                        p.play();
                                    } else {
                                        p.pause();
                                    }
                                }
                            }
                        }
                        stream = Some(s);
                        player = Some(p);
                    }
                    None => {
                        // No output device (yet) or the stream could not be
                        // built. Retry after a pause rather than dying, so a
                        // device that appears later is picked up.
                        thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }

            let Some(stream) = stream.as_ref() else {
                continue;
            };
            let Some(player) = player.as_mut() else {
                continue;
            };

            let (is_playing, current, volume, rg_enabled) = {
                let c = controller.lock().unwrap();
                (
                    c.is_playing,
                    c.current_track.clone(),
                    c.volume,
                    c.replaygain,
                )
            };

            // Recompute the ReplayGain multiplier when the track or the
            // setting changes (covers mid-track toggles). Compare by
            // reference so the loop doesn't allocate a String every tick.
            if current.as_ref() != rg_key.0.as_ref() || rg_enabled != rg_key.1 {
                rg_key = (current.clone(), rg_enabled);
                current_rg_multiplier = if rg_enabled {
                    current
                        .as_deref()
                        .and_then(|id| controller.lock().unwrap().store.get_path(id))
                        .map(|p| track_gain_multiplier(Path::new(&p)))
                        .unwrap_or(1.0)
                } else {
                    1.0
                };
            }

            player.set_volume(volume * current_rg_multiplier);

            // Publish the current playback position for the UI. A pending
            // seek is shown as the position immediately, so the UI doesn't
            // wait for the (deferred) rodio seek to land.
            let pos = player.get_pos().as_millis() as u64;
            state.set_position(pending_seek.unwrap_or(pos));

            if !is_playing {
                player.pause();
                if !stream_paused {
                    let _ = stream.pause();
                    stream_paused = true;
                }
                // The stream is corked while paused, and rodio's try_seek
                // blocks until the source is polled again — remember the
                // seek and apply it on resume instead of stalling here.
                if let Some(ms) = state.take_seek() {
                    pending_seek = Some(ms);
                }
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            if stream_paused {
                let _ = stream.play();
                stream_paused = false;
            }
            player.play();

            // The stream is uncorked here, so rodio's deferred seek can run.
            if let Some(ms) = state.take_seek() {
                pending_seek = Some(ms);
            }
            if playing_id.is_some()
                && !player.empty()
                && let Some(ms) = pending_seek.take()
            {
                let _ = player.try_seek(Duration::from_millis(ms));
            }

            // When no track is current but we should be playing, pull the
            // next track from the queue (explicit or dynamic).
            let current = if current.is_none() {
                controller.lock().unwrap().determine_next_track()
            } else {
                current
            };

            // Nothing to play: drop any stale seek so it doesn't apply to
            // the next explicitly chosen track.
            if current.is_none() && playing_id.is_none() {
                pending_seek = None;
            }

            // A new track was chosen (play/skip/previous).
            if current != playing_id {
                if let Some(prev) = playing_id.take() {
                    // Scrobble on the actual listened position, not wall
                    // clock: pausing and seeking must not count toward the
                    // threshold.
                    if threshold.is_some_and(|t| player.get_pos() >= t) {
                        controller.lock().unwrap().on_track_completed(&prev);
                    }
                    player.skip_one();
                    thread::sleep(Duration::from_millis(10));
                }

                if let Some(id) = &current {
                    let store = controller.lock().unwrap().store.clone();
                    let track = store.get_track(id);
                    match track
                        .as_ref()
                        .and_then(|t| open_source(t, &store, &network, &bus))
                    {
                        Some(src) => {
                            let dur = track.as_ref().map(|t| t.duration_secs).unwrap_or(0);
                            threshold = scrobble_threshold(dur);
                            player.append(src);
                            player.play();
                            playing_id = Some(id.clone());
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
                if threshold.is_some_and(|t| player.get_pos() >= t) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrobble_threshold_scales_with_duration() {
        // Unknown duration: fall back to 30 seconds of listening.
        assert_eq!(scrobble_threshold(0), Some(Duration::from_secs(30)));
        // Too short to count at all.
        assert_eq!(scrobble_threshold(29), None);
        // Half the duration, capped at 4 minutes.
        assert_eq!(scrobble_threshold(30), Some(Duration::from_secs(15)));
        assert_eq!(scrobble_threshold(120), Some(Duration::from_secs(60)));
        assert_eq!(scrobble_threshold(600), Some(Duration::from_secs(240)));
        assert_eq!(scrobble_threshold(3600), Some(Duration::from_secs(240)));
    }
}
