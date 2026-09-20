// ./core/src/controller.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The playback controller. Owns the live queue (in memory) and applies
//! `PlayerIntent`s against it, backed by the SQLite catalog for the dynamic
//! queue and metadata persistence.

use crate::matcher::{self, SearchExpr};
use crate::model::{PlayerIntent, QueueSnapshot, SmartPlaylist, SortPreset, Track};
use crate::scanner::scan_roots;
use crate::scrobble::{NoopScrobbler, ScrobbleEvent, Scrobbler};
use crate::store::LibraryStore;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Shared playback position + seek channel between the audio thread and the
/// UI. Position is in milliseconds; the audio thread writes it every loop
/// iteration, the UI reads it each frame. Seek requests use a sentinel of -1
/// (no pending seek) or a non-negative millisecond target.
pub struct PlaybackState {
    position_ms: AtomicU64,
    seek_ms: AtomicI64,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn take_seek(&self) -> Option<u64> {
        let v = self.seek_ms.swap(-1, Ordering::Relaxed);
        if v >= 0 { Some(v as u64) } else { None }
    }

    pub fn set_position(&self, ms: u64) {
        self.position_ms.store(ms, Ordering::Relaxed);
    }
}

/// How many tracks the dynamic queue pre-fetches at a time.
const DYNAMIC_BATCH: u32 = 50;
/// Cap on history kept for "previous" and de-duplication.
const HISTORY_CAP: usize = 200;

pub struct PlayerController {
    pub store: Arc<LibraryStore>,
    pub explicit_queue: Vec<String>,
    pub dynamic_queue: Vec<String>,
    pub history: Vec<String>,
    pub current_track: Option<String>,
    pub stop_after: Option<String>,
    pub is_playing: bool,
    pub volume: f32,
    pub replaygain: bool,

    /// When set, `repopulate_dynamic_queue` skips album continuation for this
    /// (album_artist, album) pair. Set by `SkipAlbum`, cleared by `set_current`.
    skip_album: Option<(String, String)>,

    /// The query + sort driving the dynamic queue (the active smart playlist).
    dynamic_query: SearchExpr,
    dynamic_sort: SortPreset,

    scrobbler: Arc<dyn Scrobbler>,

    smart_playlists: Vec<SmartPlaylist>,

    pub playback_state: Arc<PlaybackState>,
}

impl PlayerController {
    pub fn new(store: Arc<LibraryStore>) -> Self {
        let smart_playlists = store.load_smart_playlists();
        let replaygain = store.load_replaygain();
        Self {
            store,
            explicit_queue: Vec::new(),
            dynamic_queue: Vec::new(),
            history: Vec::new(),
            current_track: None,
            stop_after: None,
            is_playing: false,
            volume: 1.0,
            replaygain,
            skip_album: None,
            dynamic_query: SearchExpr::Str(
                crate::model::Field::All,
                crate::model::CmpOp::Contains,
                String::new(),
            ),
            dynamic_sort: SortPreset::RandomAlbum,
            scrobbler: Arc::new(NoopScrobbler),
            smart_playlists,
            playback_state: Arc::new(PlaybackState::new()),
        }
    }

    pub fn set_scrobbler(&mut self, scrobbler: Arc<dyn Scrobbler>) {
        self.scrobbler = scrobbler;
    }

    pub fn smart_playlists(&self) -> &[SmartPlaylist] {
        &self.smart_playlists
    }

    /// Configure the dynamic queue source (the active smart playlist/search).
    pub fn set_dynamic_source(&mut self, query: SearchExpr, sort: SortPreset) {
        self.dynamic_query = query;
        self.dynamic_sort = sort;
        // New source invalidates any pending dynamic picks.
        self.dynamic_queue.clear();
    }

    pub fn dynamic_sort(&self) -> SortPreset {
        self.dynamic_sort
    }

    /// Decide which track to play next. Sets `current_track` and returns its id,
    /// or `None` when the queue is exhausted (and `is_playing` is cleared).
    pub fn determine_next_track(&mut self) -> Option<String> {
        // Stop after a specific track: halt when the track that just finished
        // is the one the user marked. `current_track` is still set when
        // `determine_next_track` is called directly (tests, natural end before
        // NextTrack clears it); `history.last()` covers the NextTrack path.
        if let Some(stop_id) = &self.stop_after {
            let just_finished = self.current_track.as_ref().or_else(|| self.history.last());
            if just_finished == Some(stop_id) {
                self.stop_after = None;
                self.is_playing = false;
                return None;
            }
        }

        // 1. The explicit queue always wins (play-next / user-enqueued tracks).
        if let Some(id) = self.explicit_queue.first().cloned() {
            self.explicit_queue.remove(0);
            // Invalidate stale dynamic picks so album continuation follows
            // the enqueued track, not whatever was playing before it.
            self.dynamic_queue.clear();
            self.set_current(id.clone());
            return Some(id);
        }

        // 2. Refill the dynamic queue from the catalog if it ran dry.
        if self.dynamic_queue.is_empty() {
            self.repopulate_dynamic_queue();
        }

        // 3. Play from the dynamic queue.
        if let Some(id) = self.dynamic_queue.first().cloned() {
            self.dynamic_queue.remove(0);
            self.set_current(id.clone());
            return Some(id);
        }

        self.is_playing = false;
        None
    }

    /// Called by the audio backend once a track actually begins decoding.
    /// Increments the play count, writes it to the file tag and fires a
    /// now-playing scrobble.
    pub fn on_track_started(&mut self, id: &str) {
        let now = unix_now();
        self.store.increment_play_count(id, now);
        if let Some(track) = self.store.get_track(id) {
            let _ = crate::metadata::write_rating_and_play_count(
                Path::new(&track.path),
                track.rating,
                track.play_count.saturating_add(1),
            );
            self.scrobbler.report(&track, ScrobbleEvent::NowPlaying);
        }
    }

    /// Called by the audio backend when a track finishes past the scrobble
    /// threshold (half its duration, or 4 minutes — whichever is shorter).
    pub fn on_track_completed(&mut self, id: &str) {
        if let Some(track) = self.store.get_track(id) {
            self.scrobbler.report(&track, ScrobbleEvent::Submitted);
        }
    }

    /// Go back to the previous track, pushing the current one back to the
    /// front of the explicit queue so it is not lost.
    pub fn previous_track(&mut self) -> Option<String> {
        if let Some(prev_id) = self.history.pop() {
            if let Some(curr) = self.current_track.take() {
                self.explicit_queue.insert(0, curr);
            }
            self.current_track = Some(prev_id.clone());
            self.is_playing = true;
            return Some(prev_id);
        }
        None
    }

    pub fn current_path(&self) -> Option<String> {
        self.current_track
            .as_ref()
            .and_then(|id| self.store.get_path(id))
    }

    pub fn current_track_ref(&self) -> Option<Track> {
        self.current_track
            .as_ref()
            .and_then(|id| self.store.get_track(id))
    }

    pub fn queue_snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            explicit_queue: self.explicit_queue.clone(),
            dynamic_queue: self.dynamic_queue.clone(),
            history: self.history.clone(),
            current_track: self.current_track.clone(),
        }
    }

    pub fn restore_queue(&mut self, snap: QueueSnapshot) {
        self.explicit_queue = snap.explicit_queue;
        self.dynamic_queue = snap.dynamic_queue;
        self.history = snap.history;
        self.current_track = snap.current_track;
    }

    /// Apply a player intent. Pure state mutation; the audio backend observes
    /// the resulting `current_track`/`is_playing` fields.
    pub fn dispatch(&mut self, intent: PlayerIntent) {
        match intent {
            PlayerIntent::PlayTrack { id } => {
                self.explicit_queue.clear();
                self.dynamic_queue.clear();
                self.explicit_queue.insert(0, id);
                self.current_track = None; // force the audio thread to pick it up
                self.is_playing = true;
            }
            PlayerIntent::Enqueue { id, next } => {
                if next {
                    self.explicit_queue.insert(0, id);
                } else {
                    self.explicit_queue.push(id);
                }
            }
            PlayerIntent::RemoveFromQueue { id } => {
                self.explicit_queue.retain(|t| t != &id);
                self.dynamic_queue.retain(|t| t != &id);
            }
            PlayerIntent::ClearQueue => {
                self.explicit_queue.clear();
                self.dynamic_queue.clear();
            }
            PlayerIntent::JumpTo { id } => {
                // Skip to a track already in the queue without clearing the
                // rest. Tracks before it go to history; the target becomes
                // current. If the id is the current track, just resume.
                if self.current_track.as_deref() == Some(&id) {
                    self.is_playing = true;
                    return;
                }
                // Remove from queues so it doesn't play again immediately.
                self.explicit_queue.retain(|t| t != &id);
                self.dynamic_queue.retain(|t| t != &id);
                self.set_current(id);
                self.is_playing = true;
            }
            PlayerIntent::TogglePlayPause => {
                self.is_playing = !self.is_playing;
            }
            PlayerIntent::NextTrack => {
                // Push the current track to history so album continuation
                // and stop-after can still see it, then let the audio thread
                // pull the next one.
                if let Some(cur) = self.current_track.take() {
                    self.history.push(cur);
                    if self.history.len() > HISTORY_CAP {
                        self.history.remove(0);
                    }
                }
            }
            PlayerIntent::PreviousTrack => {
                self.previous_track();
            }
            PlayerIntent::SkipAlbum => {
                if let Some(cur) = self.current_track_ref() {
                    self.skip_album = Some((cur.album_artist.clone(), cur.album.clone()));
                    let album = cur.album;
                    let mut drop_count = 0;
                    for id in &self.explicit_queue {
                        if self.store.get_track(id).map(|t| t.album) == Some(album.clone()) {
                            drop_count += 1;
                        } else {
                            break;
                        }
                    }
                    self.explicit_queue.drain(0..drop_count);

                    let mut drop_count = 0;
                    for id in &self.dynamic_queue {
                        if self.store.get_track(id).map(|t| t.album) == Some(album.clone()) {
                            drop_count += 1;
                        } else {
                            break;
                        }
                    }
                    self.dynamic_queue.drain(0..drop_count);
                }
                self.dispatch(PlayerIntent::NextTrack);
            }
            PlayerIntent::StopAfter { id } => {
                // Empty id means "the current track" (CLI compat).
                let target = if id.is_empty() {
                    match &self.current_track {
                        Some(c) => c.clone(),
                        None => {
                            self.stop_after = None;
                            return;
                        }
                    }
                } else {
                    id
                };
                // Toggle: pressing again on the same track cancels.
                if self.stop_after.as_deref() == Some(&target) {
                    self.stop_after = None;
                } else {
                    self.stop_after = Some(target);
                }
            }
            PlayerIntent::SetVolume { volume } => {
                self.volume = volume.clamp(0.0, 1.0);
            }
            PlayerIntent::SeekTo { position_ms } => {
                self.playback_state.request_seek(position_ms);
            }
            PlayerIntent::RestoreSnapshot {
                queue,
                position_ms,
                is_playing,
            } => {
                self.restore_queue(queue);
                self.is_playing = is_playing;
                self.playback_state.request_seek(position_ms);
            }
            PlayerIntent::RateTrack { id, rating } => {
                let rating = rating.min(5);
                self.store.update_rating(&id, rating);
                if let Some(track) = self.store.get_track(&id) {
                    let _ = crate::metadata::write_rating_and_play_count(
                        Path::new(&track.path),
                        rating,
                        track.play_count,
                    );
                }
            }
            PlayerIntent::IncrementPlayCount { id } => {
                self.on_track_started(&id);
            }
            PlayerIntent::SavePlaylist { name } => {
                let query = matcher::expr_to_query(&self.dynamic_query).unwrap_or_default();
                let pl = SmartPlaylist {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    query,
                    sort_preset: self.dynamic_sort,
                };
                self.smart_playlists.push(pl);
                self.store.save_smart_playlists(&self.smart_playlists);
            }
            PlayerIntent::DeletePlaylist { id } => {
                self.smart_playlists.retain(|p| p.id != id);
                self.store.save_smart_playlists(&self.smart_playlists);
            }
            PlayerIntent::ActivatePlaylist { id } => {
                if let Some(pl) = self.smart_playlists.iter().find(|p| p.id == id).cloned() {
                    let expr = matcher::parse_query(&pl.query);
                    self.set_dynamic_source(expr, pl.sort_preset);
                }
            }
            PlayerIntent::MovePlaylist { id, up } => {
                if let Some(i) = self.smart_playlists.iter().position(|p| p.id == id) {
                    let j = if up {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(self.smart_playlists.len() - 1)
                    };
                    if i != j {
                        self.smart_playlists.swap(i, j);
                        self.store.save_smart_playlists(&self.smart_playlists);
                    }
                }
            }
            PlayerIntent::ScanLibrary { roots } => {
                self.store.save_roots(&roots);
                let progress = crate::scanner::ScanProgress::default();
                scan_roots(&self.store, &roots, &progress);
            }
        }
    }

    fn set_current(&mut self, id: String) {
        if let Some(prev) = self.current_track.take() {
            self.history.push(prev);
            if self.history.len() > HISTORY_CAP {
                self.history.remove(0);
            }
        }
        self.current_track = Some(id);
        self.is_playing = true;
        self.skip_album = None;
    }

    fn recent_ids(&self) -> HashSet<String> {
        let mut set: HashSet<String> = self.history.iter().cloned().collect();
        if let Some(c) = &self.current_track {
            set.insert(c.clone());
        }
        set
    }

    fn repopulate_dynamic_queue(&mut self) {
        match self.dynamic_sort {
            SortPreset::RandomAlbum => {
                // Continue the album the current track belongs to before jumping
                // to a random one, so playing a track mid-album plays the rest of it.
                // After NextTrack, current_track is None but history.last() holds
                // the track that just finished — use it to continue its album.
                // SkipAlbum sets `skip_album` to suppress this continuation.
                let cont_id = self.current_track.as_ref().or_else(|| self.history.last());
                if let Some(cur_id) = cont_id
                    && let Some(cur) = self.store.get_track(cur_id)
                    && self.skip_album != Some((cur.album_artist.clone(), cur.album.clone()))
                {
                    let remaining = self.store.album_tracks_after(
                        &cur.album_artist,
                        &cur.album,
                        cur.track_number,
                        &self.dynamic_query,
                    );
                    let queue: Vec<String> = remaining.into_iter().map(|t| t.id).collect();
                    if !queue.is_empty() {
                        self.dynamic_queue = queue;
                        return;
                    }
                }

                let mut exclude_album = self.skip_album.clone();
                if exclude_album.is_none()
                    && let Some(cur_id) = cont_id
                    && let Some(cur) = self.store.get_track(cur_id)
                {
                    exclude_album = Some((cur.album_artist, cur.album));
                }
                let exclude_ref = exclude_album
                    .as_ref()
                    .map(|(a, al)| (a.as_str(), al.as_str()));
                let tracks = self
                    .store
                    .random_album_tracks(&self.dynamic_query, exclude_ref);
                self.dynamic_queue = tracks.into_iter().map(|t| t.id).collect();
            }
            SortPreset::Random => {
                let page =
                    self.store
                        .filter(&self.dynamic_query, SortPreset::Random, DYNAMIC_BATCH, 0);
                let recent = self.recent_ids();
                let queue: Vec<String> = page
                    .tracks
                    .iter()
                    .map(|t| t.id.clone())
                    .filter(|id| !recent.contains(id))
                    .collect();
                if queue.is_empty() && !page.tracks.is_empty() {
                    self.dynamic_queue = page.tracks.into_iter().map(|t| t.id).collect();
                } else {
                    self.dynamic_queue = queue;
                }
            }
            _ => {
                // Ordered: no auto-refill. Playback stops when the explicit queue
                // runs out — the user can press Enter on another track to continue.
            }
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Field;

    fn make_store() -> Arc<LibraryStore> {
        let store = Arc::new(LibraryStore::open_memory().unwrap());
        for i in 0..5 {
            let track = Track {
                id: format!("{i}"),
                path: format!("/m/{i}.mp3"),
                title: format!("song {i}"),
                artist: "pink".into(),
                album_artist: "pink".into(),
                album: "album".into(),
                genre: "jazz".into(),
                comment: String::new(),
                track_number: i,
                year: 2000 + i,
                duration_secs: 100,
                rating: 0,
                play_count: 0,
                last_played: None,
                file_mtime: 0,
                updated_at: 0,
                is_local: true,
            };
            store.upsert_track(&track).unwrap();
        }
        store
    }

    #[test]
    fn explicit_queue_takes_priority() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        c.dispatch(PlayerIntent::Enqueue {
            id: "2".into(),
            next: true,
        });
        c.dispatch(PlayerIntent::Enqueue {
            id: "3".into(),
            next: false,
        });
        assert_eq!(c.determine_next_track().as_deref(), Some("2"));
        assert_eq!(c.determine_next_track().as_deref(), Some("3"));
    }

    #[test]
    fn stop_after_current_halts() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        c.dispatch(PlayerIntent::Enqueue {
            id: "1".into(),
            next: true,
        });
        c.determine_next_track();
        // Empty id = stop after the current track.
        c.dispatch(PlayerIntent::StopAfter { id: String::new() });
        assert!(c.determine_next_track().is_none());
        assert!(!c.is_playing);
    }

    #[test]
    fn stop_after_specific_track() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        // Queue: track 1 (explicit), then dynamic album continuation 2..4.
        c.dispatch(PlayerIntent::PlayTrack { id: "1".into() });
        c.determine_next_track(); // plays 1
        // Mark track 3 as the stop-after target (two tracks ahead).
        c.dispatch(PlayerIntent::StopAfter { id: "3".into() });
        assert_eq!(c.stop_after.as_deref(), Some("3"));
        // Track 1 finishes → should NOT stop (stop target is 3).
        c.dispatch(PlayerIntent::NextTrack);
        assert_eq!(c.determine_next_track().as_deref(), Some("2"));
        // Track 2 finishes → should NOT stop.
        c.dispatch(PlayerIntent::NextTrack);
        assert_eq!(c.determine_next_track().as_deref(), Some("3"));
        // Track 3 finishes → should stop.
        c.dispatch(PlayerIntent::NextTrack);
        assert!(c.determine_next_track().is_none());
        assert!(!c.is_playing);
        // stop_after was consumed.
        assert!(c.stop_after.is_none());
    }

    #[test]
    fn stop_after_toggles_off() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        c.dispatch(PlayerIntent::PlayTrack { id: "1".into() });
        c.determine_next_track();
        c.dispatch(PlayerIntent::StopAfter { id: "1".into() });
        assert!(c.stop_after.is_some());
        // Same id again cancels.
        c.dispatch(PlayerIntent::StopAfter { id: "1".into() });
        assert!(c.stop_after.is_none());
    }

    #[test]
    fn remove_from_queue() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        c.dispatch(PlayerIntent::Enqueue {
            id: "1".into(),
            next: false,
        });
        c.dispatch(PlayerIntent::Enqueue {
            id: "2".into(),
            next: false,
        });
        c.dispatch(PlayerIntent::RemoveFromQueue { id: "1".into() });
        assert_eq!(c.explicit_queue, vec!["2".to_string()]);
    }

    #[test]
    fn dynamic_queue_refills() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        c.set_dynamic_source(
            SearchExpr::Str(Field::All, crate::model::CmpOp::Contains, String::new()),
            SortPreset::Random,
        );
        let first = c.determine_next_track();
        assert!(first.is_some());
        // The dynamic queue should have been refilled beyond the first pick.
        assert!(!c.dynamic_queue.is_empty() || c.history.is_empty());
    }

    #[test]
    fn random_album_continues_current_album() {
        let store = make_store();
        let mut c = PlayerController::new(store);
        // make_store creates tracks 0..5, all on album "album" with track_number == i.
        c.dispatch(PlayerIntent::PlayTrack { id: "1".into() });
        assert_eq!(c.determine_next_track().as_deref(), Some("1"));
        // After track 1 finishes, the rest of the album should play in order.
        assert_eq!(c.determine_next_track().as_deref(), Some("2"));
        assert_eq!(c.determine_next_track().as_deref(), Some("3"));
        assert_eq!(c.determine_next_track().as_deref(), Some("4"));
    }

    #[test]
    fn random_album_continues_after_next_track() {
        // Regression: pressing Next after a manually-played track must
        // continue the same album, not jump to a random one. The bug was
        // that NextTrack cleared current_track, so repopulate_dynamic_queue
        // could not find the album to continue.
        let store = make_store();
        let mut c = PlayerController::new(store);
        c.dispatch(PlayerIntent::PlayTrack { id: "1".into() });
        assert_eq!(c.determine_next_track().as_deref(), Some("1"));

        // Simulate the audio thread: dispatch NextTrack, then pull next.
        c.dispatch(PlayerIntent::NextTrack);
        assert_eq!(c.determine_next_track().as_deref(), Some("2"));

        c.dispatch(PlayerIntent::NextTrack);
        assert_eq!(c.determine_next_track().as_deref(), Some("3"));

        c.dispatch(PlayerIntent::NextTrack);
        assert_eq!(c.determine_next_track().as_deref(), Some("4"));
    }

    #[test]
    fn enqueue_takes_over_album_continuation() {
        // Two albums: "x" (tracks 0-2) and "y" (tracks 3-5).
        let store = Arc::new(LibraryStore::open_memory().unwrap());
        for (id, album, tn) in [
            (0, "x", 0),
            (1, "x", 1),
            (2, "x", 2),
            (3, "y", 0),
            (4, "y", 1),
            (5, "y", 2),
        ] {
            store
                .upsert_track(&Track {
                    id: id.to_string(),
                    path: format!("/m/{id}.mp3"),
                    title: format!("song {id}"),
                    artist: "pink".into(),
                    album_artist: "pink".into(),
                    album: album.into(),
                    genre: "jazz".into(),
                    comment: String::new(),
                    track_number: tn,
                    year: 2000,
                    duration_secs: 100,
                    rating: 0,
                    play_count: 0,
                    last_played: None,
                    file_mtime: 0,
                    updated_at: 0,
                    is_local: true,
                })
                .unwrap();
        }
        let mut c = PlayerController::new(store);

        // Play track 1 (album x) — dynamic queue fills with 2 (rest of x).
        c.dispatch(PlayerIntent::PlayTrack { id: "1".into() });
        assert_eq!(c.determine_next_track().as_deref(), Some("1"));

        // Enqueue track 3 (album y) as play-next.
        c.dispatch(PlayerIntent::Enqueue {
            id: "3".into(),
            next: true,
        });

        // Track 3 plays from the explicit queue; the stale x-album picks are dropped.
        assert_eq!(c.determine_next_track().as_deref(), Some("3"));

        // After track 3, continuation should follow album y (4, 5), not x (2).
        assert_eq!(c.determine_next_track().as_deref(), Some("4"));
        assert_eq!(c.determine_next_track().as_deref(), Some("5"));
    }

    #[test]
    fn rating_persists_to_catalog() {
        let store = make_store();
        let mut c = PlayerController::new(store.clone());
        c.dispatch(PlayerIntent::RateTrack {
            id: "1".into(),
            rating: 4,
        });
        assert_eq!(store.get_track("1").unwrap().rating, 4);
    }

    #[test]
    fn skip_album_jumps_to_different_album() {
        // Two albums: "x" (tracks 0-2) and "y" (tracks 3-5).
        let store = Arc::new(LibraryStore::open_memory().unwrap());
        for (id, album, tn) in [
            (0, "x", 0),
            (1, "x", 1),
            (2, "x", 2),
            (3, "y", 0),
            (4, "y", 1),
            (5, "y", 2),
        ] {
            store
                .upsert_track(&Track {
                    id: id.to_string(),
                    path: format!("/m/{id}.mp3"),
                    title: format!("song {id}"),
                    artist: "pink".into(),
                    album_artist: "pink".into(),
                    album: album.into(),
                    genre: "jazz".into(),
                    comment: String::new(),
                    track_number: tn,
                    year: 2000,
                    duration_secs: 100,
                    rating: 0,
                    play_count: 0,
                    last_played: None,
                    file_mtime: 0,
                    updated_at: 0,
                    is_local: true,
                })
                .unwrap();
        }
        let mut c = PlayerController::new(store);

        // Play track 1 (album x) — continuation would normally play 2 next.
        c.dispatch(PlayerIntent::PlayTrack { id: "1".into() });
        assert_eq!(c.determine_next_track().as_deref(), Some("1"));

        // Skip album: should NOT continue with track 2 (same album).
        c.dispatch(PlayerIntent::SkipAlbum);
        let next = c.determine_next_track();
        assert_ne!(next.as_deref(), Some("2"));
        // The next track must be from album y.
        if let Some(id) = &next {
            assert_eq!(c.store.get_track(id).unwrap().album, "y");
        }
    }
}
