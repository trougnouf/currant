// ./core/src/scrobble.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Scrobbling. A trait-based hook so Last.fm and Listenbrainz plug in the same
//! way. The controller emits events on playback start (now-playing) and on
//! track completion (submission); frontends wire a concrete scrobbler.

use crate::model::Track;

/// One scrobble lifecycle event.
#[derive(Debug, Clone)]
pub enum ScrobbleEvent {
    /// Track started playing; send a "now playing" update.
    NowPlaying,
    /// Track played past the submission threshold; record it.
    Submitted,
}

/// A scrobbler backend. Implementations do the network I/O; the controller
/// just calls these and ignores errors (scrobbling must never block playback).
pub trait Scrobbler: Send + Sync {
    fn report(&self, track: &Track, event: ScrobbleEvent);
}

/// A no-op scrobbler for when scrobbling is disabled.
pub struct NoopScrobbler;

impl Scrobbler for NoopScrobbler {
    fn report(&self, _track: &Track, _event: ScrobbleEvent) {}
}

/// A Listenbrainz scrobbler. Submits "now playing" and "listen" requests to
/// the configured API root using the user's API token.
pub struct ListenbrainzScrobbler {
    api_root: String,
    token: String,
}

impl ListenbrainzScrobbler {
    pub fn new(token: String) -> Self {
        Self {
            api_root: "https://api.listenbrainz.org".to_string(),
            token,
        }
    }

    pub fn with_api_root(api_root: String, token: String) -> Self {
        Self { api_root, token }
    }

    fn submit(&self, track: &Track, event: &ScrobbleEvent) {
        let listen_type = match event {
            ScrobbleEvent::NowPlaying => "playing_now",
            ScrobbleEvent::Submitted => "single",
        };
        let payload = match event {
            ScrobbleEvent::Submitted => serde_json::json!({
                "listen_type": listen_type,
                "payload": [{
                    "listened_at": now_unix(),
                    "track_metadata": track_metadata(track),
                }],
            }),
            ScrobbleEvent::NowPlaying => serde_json::json!({
                "listen_type": listen_type,
                "payload": [{
                    "track_metadata": track_metadata(track),
                }],
            }),
        };
        let url = format!("{}/1/submit-listens", self.api_root);
        let _ = ureq::post(&url)
            .set("Authorization", &format!("Token {}", self.token))
            .send_json(payload);
    }
}

impl Scrobbler for ListenbrainzScrobbler {
    fn report(&self, track: &Track, event: ScrobbleEvent) {
        self.submit(track, &event);
    }
}

fn track_metadata(track: &Track) -> serde_json::Value {
    let mut artist = track.artist.clone();
    if artist == "Unknown Artist" {
        artist.clear();
    }
    serde_json::json!({
        "artist_name": artist,
        "track_name": track.title,
        "release_name": track.album,
        "additional_info": {
            "tracknumber": track.track_number,
            "duration": track.duration_secs,
        },
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
