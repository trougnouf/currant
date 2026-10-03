// ./tui/src/audio.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The audio backend for the TUI. Owns the rodio output stream and drives
//! playback from the controller's live queue. Opus falls back to the bundled
//! libopus decoder; everything else is decoded by symphonia via rodio.

use currant_core::controller::PlaybackState;
use currant_core::controller::PlayerController;
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

/// Open a track as a rodio `Source`. Local files are opened directly; remote
/// tracks are streamed from the peer that owns them through a seekable HTTP
/// reader. Tries symphonia first, then the opus decoder when the `opus`
/// feature is enabled.
fn open_source(
    track: &Track,
    store: &LibraryStore,
    network: &Arc<Mutex<NetworkState>>,
) -> Option<Box<dyn Source<Item = f32> + Send>> {
    // A fresh seekable reader per attempt: each decoder consumes its reader,
    // so a failed first attempt must not leave the second one starting
    // mid-file.
    let make_reader = || -> Option<Box<dyn Seekable>> {
        if track.is_local {
            File::open(&track.path)
                .ok()
                .map(|f| Box::new(f) as Box<dyn Seekable>)
        } else {
            let peer_id = store.get_remote_source_peer(&track.id)?;
            let peer = {
                let state = network.lock().unwrap();
                state.peers.get(&peer_id)?.clone()
            };
            Some(Box::new(
                HttpSeekableReader::new(
                    &peer.ips,
                    peer.http_port,
                    &track.id,
                    store.load_pairing_token(),
                )
                .ok()?,
            ))
        }
    };

    if let Some(reader) = make_reader()
        && let Ok(decoder) = Decoder::new(reader)
    {
        return Some(Box::new(decoder));
    }
    #[cfg(feature = "opus")]
    {
        let is_opus = Path::new(&track.path)
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
    eprintln!("failed to open track {}: {}", track.id, track.path);
    None
}

/// Spawn the audio thread. It owns the output stream and polls the controller.
pub fn spawn(
    controller: Arc<Mutex<PlayerController>>,
    state: Arc<PlaybackState>,
    network: Arc<Mutex<NetworkState>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let host = rodio::cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => return,
        };
        let supported_config = match device.default_output_config() {
            Ok(c) => c,
            Err(_) => return,
        };
        let sample_format = supported_config.sample_format();
        let config: rodio::cpal::StreamConfig = supported_config.into();

        let (mixer_ctrl, mut mixer_out) = match (
            NonZero::new(config.channels),
            NonZero::new(config.sample_rate),
        ) {
            (Some(channels), Some(sample_rate)) => rodio::mixer::mixer(channels, sample_rate),
            _ => return,
        };
        let err_fn = |err| eprintln!("audio stream error: {err}");

        // Feed the mixer output into a raw cpal stream so we keep ownership
        // of the stream and can cork it at the OS level while paused.
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
                        *sample = (mixer_out.next().unwrap_or(0.0).clamp(-1.0, 1.0)
                            * (i32::MAX as f32)) as i32;
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
            _ => return, // Unsupported sample format
        };
        let stream = match stream {
            Ok(s) => s,
            Err(_) => return,
        };
        if stream.play().is_err() {
            return;
        }

        let player = Player::connect_new(&mixer_ctrl);

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
            // setting changes (covers mid-track toggles).
            let key = (current.clone(), rg_enabled);
            if key != rg_key {
                rg_key = key;
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
                        .and_then(|t| open_source(t, &store, &network))
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
