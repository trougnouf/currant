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
    /// Shared HTTP agent with a bounded timeout, so a slow or unreachable
    /// server can never hang a scrobble thread.
    agent: ureq::Agent,
}

impl ListenbrainzScrobbler {
    pub fn new(token: String) -> Self {
        Self::with_api_root("https://api.listenbrainz.org".to_string(), token)
    }

    pub fn with_api_root(api_root: String, token: String) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                .build(),
        );
        Self {
            api_root,
            token,
            agent,
        }
    }
}

impl Scrobbler for ListenbrainzScrobbler {
    /// Fire-and-forget: the HTTP call runs on a short-lived thread so a slow
    /// or unreachable server never blocks playback.
    fn report(&self, track: &Track, event: ScrobbleEvent) {
        let agent = self.agent.clone();
        let api_root = self.api_root.clone();
        let token = self.token.clone();
        let track = track.clone();
        std::thread::spawn(move || submit(&agent, &api_root, &token, &track, &event));
    }
}

/// POST one listen to the Listenbrainz API. Errors are ignored — scrobbling
/// must never affect playback.
fn submit(agent: &ureq::Agent, api_root: &str, token: &str, track: &Track, event: &ScrobbleEvent) {
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
    let url = format!("{api_root}/1/submit-listens");
    let _ = agent
        .post(&url)
        .header("Authorization", &format!("Token {token}"))
        .send_json(&payload);
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
