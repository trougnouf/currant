// ./tui/src/app.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! TUI application state: tabs, search, sort, view presets, windowed caches
//! and the key bindings that drive the controller.

use crate::audio::PlaybackState;
use cassis_core::controller::PlayerController;
use cassis_core::matcher::{self, parse_query};
use cassis_core::model::{Album, Artist, PlayerIntent, SmartPlaylist, SortPreset, Track};
use cassis_core::scanner::ScanProgress;
use cassis_core::store::LibraryStore;
use std::sync::{Arc, MutexGuard};

/// Library view tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tracks,
    Albums,
    Artists,
    Queue,
    Playlists,
    Files,
}

impl Tab {
    const ALL: [Tab; 6] = [
        Tab::Tracks,
        Tab::Albums,
        Tab::Artists,
        Tab::Queue,
        Tab::Playlists,
        Tab::Files,
    ];

    fn next(self) -> Tab {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap();
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Tab {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap();
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Column presets, cycled with `c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewPreset {
    Minimal, // title + duration
    Compact, // + artist + album
    Full,    // + rating + year + genre
}

/// One row in the queue tab.
#[derive(Debug, Clone)]
pub struct QueueRow {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub kind: QueueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    NowPlaying,
    Explicit,
    Dynamic,
}

/// Tracks of an expanded album or artist. `v` toggles in/out.
#[derive(Debug, Clone)]
pub struct ExpandedView {
    pub kind: ExpandKind,
    pub tracks: Vec<Track>,
    pub selection: usize,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandKind {
    Album,
    Artist,
}

/// A windowed cache of tracks so 100k+ libraries stay smooth.
struct WindowedView {
    offset: usize,
    items: Vec<Track>,
    total: u64,
    selection: usize,
}

impl WindowedView {
    const WINDOW: u32 = 800;

    fn new() -> Self {
        Self {
            offset: 0,
            items: Vec::new(),
            total: 0,
            selection: 0,
        }
    }

    fn invalidate(&mut self) {
        self.items.clear();
        self.offset = 0;
        self.total = 0;
        self.selection = 0;
        // A new search/sort produces a completely different result set;
        // the old selection and total are meaningless.
    }

    fn clamp(&mut self) {
        if self.total == 0 {
            self.selection = 0;
        } else {
            self.selection = self.selection.min(self.total as usize - 1);
        }
    }

    /// Reload the window if the selection has moved outside the loaded range.
    fn ensure(&mut self, store: &LibraryStore, expr: &matcher::SearchExpr, sort: SortPreset) {
        let out_of_window = self.items.is_empty()
            || self.selection < self.offset
            || self.selection >= self.offset + self.items.len();
        if out_of_window {
            let half = Self::WINDOW as usize / 2;
            let new_offset = self.selection.saturating_sub(half);
            let page = store.filter(expr, sort, Self::WINDOW, new_offset as u32);
            self.offset = new_offset;
            self.items = page.tracks;
            self.total = page.total;
            self.clamp();
        }
    }

    fn list_index(&self) -> usize {
        self.selection.saturating_sub(self.offset)
    }
}

pub struct App {
    pub tab: Tab,
    pub search: String,
    pub in_search: bool,
    pub sort: SortPreset,
    pub view: ViewPreset,
    pub status: String,
    pub help: bool,
    pub details: Option<Track>,

    tracks: WindowedView,
    files: WindowedView,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    sel_albums: usize,
    sel_artists: usize,
    sel_queue: usize,
    sel_playlists: usize,
    queue_rows: Vec<QueueRow>,

    /// Expanded album/artist view: the selected album/artist's tracks.
    expanded: Option<ExpandedView>,

    /// Last seen now-playing track id, to detect track changes for auto-jump.
    prev_playing_id: Option<String>,

    // view model rebuilt each refresh
    pub now_playing: Option<Track>,
    pub is_playing: bool,
    pub stop_after: bool,
    pub track_count: u64,
    pub radio_sort: Option<SortPreset>,
    pub volume: f32,
    pub smart_playlists: Vec<SmartPlaylist>,

    /// Shared playback state (position + seek channel) with the audio thread.
    playback: Option<Arc<PlaybackState>>,

    /// True when `g` was pressed and we expect a digit to activate a playlist.
    pending_g: bool,

    /// Live scan progress; `None` when no scan is running.
    scan_progress: Option<Arc<ScanProgress>>,

    dirty: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            tab: Tab::Tracks,
            search: String::new(),
            in_search: false,
            sort: SortPreset::ArtistAlbumTrack,
            view: ViewPreset::Compact,
            status: String::new(),
            help: false,
            details: None,
            tracks: WindowedView::new(),
            files: WindowedView::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            sel_albums: 0,
            sel_artists: 0,
            sel_queue: 0,
            sel_playlists: 0,
            queue_rows: Vec::new(),
            expanded: None,
            prev_playing_id: None,
            now_playing: None,
            is_playing: false,
            stop_after: false,
            track_count: 0,
            radio_sort: None,
            volume: 0.7,
            smart_playlists: Vec::new(),
            playback: None,
            pending_g: false,
            scan_progress: None,
            dirty: true,
        }
    }

    pub fn set_scan_progress(&mut self, p: Arc<ScanProgress>) {
        self.scan_progress = Some(p);
    }

    pub fn set_playback(&mut self, p: Arc<PlaybackState>) {
        self.playback = Some(p);
    }

    pub fn position_ms(&self) -> u64 {
        self.playback.as_ref().map(|p| p.position_ms()).unwrap_or(0)
    }

    /// Rebuild caches and the view model from the controller. Called each frame.
    pub fn refresh(&mut self, c: &MutexGuard<'_, PlayerController>) {
        let expr = parse_query(&self.search);
        self.now_playing = c.current_track_ref();
        self.is_playing = c.is_playing;
        self.stop_after = c.stop_after_current;
        self.track_count = c.store.track_count();
        self.radio_sort = Some(c.dynamic_sort());
        self.volume = c.volume;
        self.smart_playlists = c.smart_playlists().to_vec();

        // Auto-jump to the playing track when it changes (next/prev/skip).
        let playing_id = self.now_playing.as_ref().map(|t| t.id.clone());
        if playing_id != self.prev_playing_id {
            self.prev_playing_id = playing_id.clone();
            // Only auto-jump when a track is playing and we're not in
            // search mode (typing would fight the selection).
            if playing_id.is_some() && !self.in_search {
                self.jump_to_playing();
            }
        }

        // While a scan is running, show live progress in the status bar.
        // Lists are NOT refreshed during the scan (the read connection serves
        // the previous catalog state via WAL, and refreshing every frame
        // would waste CPU competing with the scan thread).
        if let Some(p) = &self.scan_progress {
            if !p.is_done() {
                let r = p.snapshot();
                self.status = format!(
                    "scanning... {} files ({} new, {} updated, {} unchanged, {} errors)",
                    r.scanned, r.added, r.updated, r.unchanged, r.errors
                );
            } else {
                let r = p.snapshot();
                self.status = format!(
                    "scan done: {} scanned, {} added, {} updated, {} removed, {} errors",
                    r.scanned, r.added, r.updated, r.removed, r.errors
                );
                self.scan_progress = None;
                // One-shot refresh so the lists pick up the new catalog state.
                self.dirty = true;
            }
        }

        match self.tab {
            Tab::Tracks => self.refresh_tracks(&c.store, &expr),
            Tab::Files => self.refresh_files(&c.store, &expr),
            Tab::Albums => self.refresh_albums(&c.store, &expr),
            Tab::Artists => self.refresh_artists(&c.store, &expr),
            Tab::Queue => self.refresh_queue(c),
            Tab::Playlists => self.refresh_playlists(),
        }
        // Collapse expanded view if the underlying album/artist list was
        // invalidated (new search, sort change, etc.).
        if self.dirty {
            self.expanded = None;
        }
        self.dirty = false;
    }

    fn refresh_tracks(&mut self, store: &LibraryStore, expr: &matcher::SearchExpr) {
        if self.dirty {
            self.tracks.invalidate();
        }
        self.tracks.ensure(store, expr, self.sort);
    }

    fn refresh_files(&mut self, store: &LibraryStore, expr: &matcher::SearchExpr) {
        if self.dirty {
            self.files.invalidate();
        }
        self.files.ensure(store, expr, SortPreset::Path);
    }

    fn refresh_albums(&mut self, store: &LibraryStore, expr: &matcher::SearchExpr) {
        if self.dirty {
            self.albums = store.albums(expr);
            self.sel_albums = self.sel_albums.min(self.albums.len().saturating_sub(1));
        }
    }

    fn refresh_artists(&mut self, store: &LibraryStore, expr: &matcher::SearchExpr) {
        if self.dirty {
            self.artists = store.artists(expr);
            self.sel_artists = self.sel_artists.min(self.artists.len().saturating_sub(1));
        }
    }

    fn refresh_queue(&mut self, c: &MutexGuard<'_, PlayerController>) {
        let snap = c.queue_snapshot();
        let store = &c.store;
        let mut rows = Vec::new();
        if let Some(id) = &snap.current_track
            && let Some(t) = store.get_track(id)
        {
            rows.push(QueueRow {
                id: id.clone(),
                title: t.title.clone(),
                artist: t.artist.clone(),
                kind: QueueKind::NowPlaying,
            });
        }
        for id in &snap.explicit_queue {
            let (title, artist) = store
                .get_track(id)
                .map(|t| (t.title, t.artist))
                .unwrap_or_default();
            rows.push(QueueRow {
                id: id.clone(),
                title,
                artist,
                kind: QueueKind::Explicit,
            });
        }
        for id in &snap.dynamic_queue {
            let (title, artist) = store
                .get_track(id)
                .map(|t| (t.title, t.artist))
                .unwrap_or_default();
            rows.push(QueueRow {
                id: id.clone(),
                title,
                artist,
                kind: QueueKind::Dynamic,
            });
        }
        self.sel_queue = self.sel_queue.min(rows.len().saturating_sub(1));
        self.queue_rows = rows;
    }

    fn refresh_playlists(&mut self) {
        self.sel_playlists = self
            .sel_playlists
            .min(self.smart_playlists.len().saturating_sub(1));
    }

    // --- accessors used by the ui ---

    pub fn tracks_view(&self) -> (&[Track], usize, u64, usize) {
        (
            &self.tracks.items,
            self.tracks.list_index(),
            self.tracks.total,
            self.tracks.offset,
        )
    }

    pub fn files_view(&self) -> (&[Track], usize, u64, usize) {
        (
            &self.files.items,
            self.files.list_index(),
            self.files.total,
            self.files.offset,
        )
    }

    pub fn albums_view(&self) -> (&[Album], usize) {
        (&self.albums, self.sel_albums)
    }

    pub fn artists_view(&self) -> (&[Artist], usize) {
        (&self.artists, self.sel_artists)
    }

    pub fn queue_view(&self) -> (&[QueueRow], usize) {
        (&self.queue_rows, self.sel_queue)
    }

    pub fn playlists_view(&self) -> (&[SmartPlaylist], usize) {
        (&self.smart_playlists, self.sel_playlists)
    }

    pub fn expanded_view(&self) -> Option<&ExpandedView> {
        self.expanded.as_ref()
    }

    // --- input ---

    /// Handle a key. Returns true to quit. `c` is the locked controller.
    pub fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        c: &mut MutexGuard<'_, PlayerController>,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Details overlay: any key closes it.
        if self.details.is_some() {
            self.details = None;
            return false;
        }

        // Search mode: capture printable input until Esc/Enter.
        if self.in_search {
            match key.code {
                KeyCode::Esc => self.in_search = false,
                KeyCode::Enter => self.in_search = false,
                KeyCode::Backspace => {
                    self.search.pop();
                    self.invalidate_list();
                }
                KeyCode::Char(ch) => {
                    self.search.push(ch);
                    self.invalidate_list();
                    self.reset_selection();
                }
                _ => {}
            }
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return true,
                KeyCode::Char('p') => c.dispatch(PlayerIntent::TogglePlayPause),
                KeyCode::Char('n') => c.dispatch(PlayerIntent::NextTrack),
                KeyCode::Char('b') => c.dispatch(PlayerIntent::PreviousTrack),
                KeyCode::Char('u') => {
                    self.search.clear();
                    self.invalidate_list();
                    self.reset_selection();
                }
                KeyCode::Char('j') => self.jump_to_playing(),
                _ => {}
            }
            return false;
        }

        // g-prefix: activate a saved playlist by index.
        if self.pending_g {
            self.pending_g = false;
            if let KeyCode::Char(ch) = key.code
                && let Some(n) = ch.to_digit(10)
            {
                let idx = n as usize;
                if idx == 0 {
                    self.status = "no playlist 0".into();
                } else if let Some(pl) = self.smart_playlists.get(idx - 1).cloned() {
                    c.dispatch(PlayerIntent::ActivatePlaylist { id: pl.id });
                    self.status = format!("activated: {pl_name}", pl_name = pl.name);
                } else {
                    self.status = format!("no playlist {n}");
                }
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.expanded = None;
                self.dirty = true;
            }
            KeyCode::BackTab => {
                self.tab = self.tab.prev();
                self.expanded = None;
                self.dirty = true;
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-20),
            KeyCode::PageDown => self.move_selection(20),
            KeyCode::Home => self.move_selection_to(0),
            KeyCode::End => self.move_selection_to(usize::MAX),
            KeyCode::Enter => self.activate(c),
            KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('p') => c.dispatch(PlayerIntent::TogglePlayPause),
            KeyCode::Char('>') | KeyCode::Char('.') => c.dispatch(PlayerIntent::NextTrack),
            KeyCode::Char('<') | KeyCode::Char(',') => c.dispatch(PlayerIntent::PreviousTrack),
            KeyCode::Char('s') => {
                c.dispatch(PlayerIntent::StopAfterCurrent);
                self.status = if c.stop_after_current {
                    "stop after current: on".into()
                } else {
                    "stop after current: off".into()
                };
            }
            KeyCode::Char('q') => self.enqueue_selected(c, false),
            KeyCode::Char('n') => self.enqueue_selected(c, true),
            KeyCode::Char('x') => self.remove_from_queue(c),
            KeyCode::Char('c') => {
                self.view = match self.view {
                    ViewPreset::Minimal => ViewPreset::Compact,
                    ViewPreset::Compact => ViewPreset::Full,
                    ViewPreset::Full => ViewPreset::Minimal,
                };
            }
            KeyCode::Char('r') => self.cycle_sort(),
            KeyCode::Char('R') => self.set_radio(c, SortPreset::RandomAlbum),
            KeyCode::Char('m') => self.set_radio(c, self.sort),
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Char('d') => self.show_details(c),
            KeyCode::Char('v') => self.toggle_expand(c),
            KeyCode::Char('P') => {
                let name = if self.search.is_empty() {
                    format!("playlist {}", self.smart_playlists.len() + 1)
                } else {
                    self.search.clone()
                };
                c.dispatch(PlayerIntent::SavePlaylist { name: name.clone() });
                self.status = format!("saved playlist: {name}");
            }
            KeyCode::Char('g') => {
                self.pending_g = true;
                self.status = "g+1-9: activate playlist".into();
            }
            KeyCode::Char('/') => {
                self.in_search = true;
                self.status = "search: ".into();
            }
            KeyCode::Char('0') => self.rate_selected(c, 0),
            KeyCode::Char('1') => self.rate_selected(c, 1),
            KeyCode::Char('2') => self.rate_selected(c, 2),
            KeyCode::Char('3') => self.rate_selected(c, 3),
            KeyCode::Char('4') => self.rate_selected(c, 4),
            KeyCode::Char('5') => self.rate_selected(c, 5),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let v = (c.volume + 0.05).min(1.0);
                c.dispatch(PlayerIntent::SetVolume { volume: v });
                self.status = format!("vol: {:0.0}%", v * 100.0);
            }
            KeyCode::Char('-') => {
                let v = (c.volume - 0.05).max(0.0);
                c.dispatch(PlayerIntent::SetVolume { volume: v });
                self.status = format!("vol: {:0.0}%", v * 100.0);
            }
            KeyCode::Left | KeyCode::Char('h') => self.seek_relative(c, -5),
            KeyCode::Right | KeyCode::Char('l') => self.seek_relative(c, 5),
            KeyCode::Char('H') => self.seek_relative(c, -30),
            KeyCode::Char('L') => self.seek_relative(c, 30),
            _ => {}
        }
        false
    }

    fn invalidate_list(&mut self) {
        self.dirty = true;
    }

    fn reset_selection(&mut self) {
        if let Some(e) = &mut self.expanded {
            e.selection = 0;
            return;
        }
        match self.tab {
            Tab::Tracks => self.tracks.selection = 0,
            Tab::Files => self.files.selection = 0,
            Tab::Albums => self.sel_albums = 0,
            Tab::Artists => self.sel_artists = 0,
            Tab::Queue => self.sel_queue = 0,
            Tab::Playlists => self.sel_playlists = 0,
        }
    }

    fn current_total(&self) -> usize {
        if self.expanded.is_some() {
            return self.expanded.as_ref().map(|e| e.tracks.len()).unwrap_or(0);
        }
        match self.tab {
            Tab::Tracks => self.tracks.total as usize,
            Tab::Files => self.files.total as usize,
            Tab::Albums => self.albums.len(),
            Tab::Artists => self.artists.len(),
            Tab::Queue => self.queue_rows.len(),
            Tab::Playlists => self.smart_playlists.len(),
        }
    }

    fn current_selection(&self) -> usize {
        if self.expanded.is_some() {
            return self.expanded.as_ref().map(|e| e.selection).unwrap_or(0);
        }
        match self.tab {
            Tab::Tracks => self.tracks.selection,
            Tab::Files => self.files.selection,
            Tab::Albums => self.sel_albums,
            Tab::Artists => self.sel_artists,
            Tab::Queue => self.sel_queue,
            Tab::Playlists => self.sel_playlists,
        }
    }

    fn set_selection(&mut self, i: usize) {
        let max = self.current_total().saturating_sub(1);
        let i = i.min(max);
        if let Some(e) = &mut self.expanded {
            e.selection = i;
            return;
        }
        match self.tab {
            Tab::Tracks => self.tracks.selection = i,
            Tab::Files => self.files.selection = i,
            Tab::Albums => self.sel_albums = i,
            Tab::Artists => self.sel_artists = i,
            Tab::Queue => self.sel_queue = i,
            Tab::Playlists => self.sel_playlists = i,
        }
        // ensure() will reload the window if the selection moved outside it.
        // No need to set dirty — that would invalidate and reset selection to 0.
    }

    fn move_selection(&mut self, delta: i64) {
        let cur = self.current_selection() as i64;
        let next = cur.saturating_add(delta).max(0) as usize;
        self.set_selection(next);
    }

    fn move_selection_to(&mut self, i: usize) {
        self.set_selection(if i == usize::MAX {
            self.current_total().saturating_sub(1)
        } else {
            i
        });
    }

    fn selected_track_id(&self, c: &MutexGuard<'_, PlayerController>) -> Option<String> {
        if let Some(e) = &self.expanded {
            return e.tracks.get(e.selection).map(|t| t.id.clone());
        }
        match self.tab {
            Tab::Tracks => self
                .tracks
                .items
                .get(self.tracks.list_index())
                .map(|t| t.id.clone()),
            Tab::Files => self
                .files
                .items
                .get(self.files.list_index())
                .map(|t| t.id.clone()),
            Tab::Queue => self.queue_rows.get(self.sel_queue).map(|r| r.id.clone()),
            Tab::Albums | Tab::Artists | Tab::Playlists => None,
        }
        .or_else(|| {
            // Albums/artists: resolve the first track of the selection.
            match self.tab {
                Tab::Albums => self.albums.get(self.sel_albums).and_then(|a| {
                    c.store
                        .album_tracks(&a.artist, &a.album)
                        .first()
                        .map(|t| t.id.clone())
                }),
                Tab::Artists => self
                    .artists
                    .get(self.sel_artists)
                    .and_then(|a| c.store.artist_tracks(&a.name).first().map(|t| t.id.clone())),
                _ => None,
            }
        })
    }

    fn activate(&mut self, c: &mut MutexGuard<'_, PlayerController>) {
        if let Some(e) = &self.expanded {
            // Expanded: play the selected individual track.
            if let Some(id) = self.selected_track_id(c) {
                c.dispatch(PlayerIntent::PlayTrack { id });
                self.status = format!("playing: {}", e.label);
            }
            return;
        }
        match self.tab {
            Tab::Tracks | Tab::Files => {
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::PlayTrack { id });
                }
            }
            Tab::Queue => {
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::JumpTo { id });
                    self.status = "jumped to track".into();
                }
            }
            Tab::Albums => {
                if let Some(a) = self.albums.get(self.sel_albums).cloned() {
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
                    play_sequence(c, tracks);
                    self.status = format!("playing album: {} - {}", a.artist, a.album);
                }
            }
            Tab::Artists => {
                if let Some(a) = self.artists.get(self.sel_artists).cloned() {
                    let tracks = c.store.artist_tracks(&a.name);
                    play_sequence(c, tracks);
                    self.status = format!("playing artist: {}", a.name);
                }
            }
            Tab::Playlists => {
                if let Some(pl) = self.smart_playlists.get(self.sel_playlists).cloned() {
                    c.dispatch(PlayerIntent::ActivatePlaylist { id: pl.id.clone() });
                    self.status = format!("activated: {}", pl.name);
                }
            }
        }
    }

    fn enqueue_selected(&mut self, c: &mut MutexGuard<'_, PlayerController>, next: bool) {
        if self.expanded.is_some() {
            if let Some(id) = self.selected_track_id(c) {
                c.dispatch(PlayerIntent::Enqueue { id, next });
                self.status = if next { "play next" } else { "queued" }.into();
            }
            return;
        }
        match self.tab {
            Tab::Tracks | Tab::Files => {
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::Enqueue { id, next });
                    self.status = if next { "play next" } else { "queued" }.into();
                }
            }
            Tab::Albums => {
                if let Some(a) = self.albums.get(self.sel_albums).cloned() {
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
                    enqueue_sequence(c, tracks, next);
                    self.status = if next { "album next" } else { "album queued" }.into();
                }
            }
            Tab::Artists => {
                if let Some(a) = self.artists.get(self.sel_artists).cloned() {
                    let tracks = c.store.artist_tracks(&a.name);
                    enqueue_sequence(c, tracks, next);
                    self.status = if next { "artist next" } else { "artist queued" }.into();
                }
            }
            Tab::Queue | Tab::Playlists => {}
        }
    }

    fn remove_from_queue(&mut self, c: &mut MutexGuard<'_, PlayerController>) {
        if self.tab == Tab::Queue {
            if let Some(row) = self.queue_rows.get(self.sel_queue).cloned() {
                c.dispatch(PlayerIntent::RemoveFromQueue { id: row.id });
                self.status = "removed from queue".into();
            }
            return;
        }
        if self.tab == Tab::Playlists
            && let Some(pl) = self.smart_playlists.get(self.sel_playlists).cloned()
        {
            c.dispatch(PlayerIntent::DeletePlaylist { id: pl.id });
            self.status = format!("deleted playlist: {}", pl.name);
        }
    }

    fn rate_selected(&mut self, c: &mut MutexGuard<'_, PlayerController>, rating: u8) {
        if let Some(id) = self.selected_track_id(c) {
            c.dispatch(PlayerIntent::RateTrack { id, rating });
            self.status = format!("rated {}/5", rating);
        }
    }

    fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            SortPreset::ArtistAlbumTrack => SortPreset::YearDesc,
            SortPreset::YearDesc => SortPreset::MostPlayed,
            SortPreset::MostPlayed => SortPreset::HighestRated,
            SortPreset::HighestRated => SortPreset::Random,
            SortPreset::Random => SortPreset::ArtistAlbumTrack,
            SortPreset::RandomAlbum => SortPreset::ArtistAlbumTrack,
            SortPreset::Path => SortPreset::ArtistAlbumTrack,
        };
        self.invalidate_list();
        self.reset_selection();
    }

    fn set_radio(&mut self, c: &mut MutexGuard<'_, PlayerController>, sort: SortPreset) {
        let expr = parse_query(&self.search);
        c.set_dynamic_source(expr, sort);
        self.status = match sort {
            SortPreset::RandomAlbum => "radio: random album".into(),
            SortPreset::Random => "radio: random".into(),
            _ => "radio: set".into(),
        };
    }

    fn show_details(&mut self, c: &MutexGuard<'_, PlayerController>) {
        if let Some(id) = self.selected_track_id(c) {
            self.details = c.store.get_track(&id);
        }
    }

    fn jump_to_playing(&mut self) {
        let Some(np) = &self.now_playing else {
            self.status = "nothing playing".into();
            return;
        };
        let id = &np.id;
        match self.tab {
            Tab::Tracks | Tab::Files => {
                // Search within the loaded window first; if not found,
                // invalidate and let the user scroll to it.
                let view = if self.tab == Tab::Tracks {
                    &self.tracks
                } else {
                    &self.files
                };
                if let Some(idx) = view.items.iter().position(|t| &t.id == id) {
                    self.set_selection(view.offset + idx);
                    self.status = "jumped to playing".into();
                } else {
                    // Not in the current window — clear the search to ensure
                    // the track is reachable, then select by id lookup.
                    self.search.clear();
                    self.invalidate_list();
                    self.status = "cleared search to find playing track".into();
                }
            }
            Tab::Albums => {
                if let Some(idx) = self
                    .albums
                    .iter()
                    .position(|a| a.artist == np.album_artist && a.album == np.album)
                {
                    self.sel_albums = idx;
                    self.expanded = None;
                    self.status = "jumped to playing album".into();
                }
            }
            Tab::Artists => {
                if let Some(idx) = self.artists.iter().position(|a| a.name == np.artist) {
                    self.sel_artists = idx;
                    self.expanded = None;
                    self.status = "jumped to playing artist".into();
                }
            }
            Tab::Queue => {
                if let Some(idx) = self.queue_rows.iter().position(|r| r.id == *id) {
                    self.sel_queue = idx;
                    self.status = "jumped to playing in queue".into();
                }
            }
            Tab::Playlists => {}
        }
    }

    fn toggle_expand(&mut self, c: &MutexGuard<'_, PlayerController>) {
        // If already expanded, collapse.
        if self.expanded.is_some() {
            self.expanded = None;
            self.status.clear();
            return;
        }
        match self.tab {
            Tab::Albums => {
                if let Some(a) = self.albums.get(self.sel_albums).cloned() {
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
                    let label = format!("{} - {}", a.artist, a.album);
                    self.expanded = Some(ExpandedView {
                        kind: ExpandKind::Album,
                        tracks,
                        selection: 0,
                        label,
                    });
                    self.status = "v: expanded album (v to collapse)".into();
                }
            }
            Tab::Artists => {
                if let Some(a) = self.artists.get(self.sel_artists).cloned() {
                    let tracks = c.store.artist_tracks(&a.name);
                    let label = a.name.clone();
                    self.expanded = Some(ExpandedView {
                        kind: ExpandKind::Artist,
                        tracks,
                        selection: 0,
                        label,
                    });
                    self.status = "v: expanded artist (v to collapse)".into();
                }
            }
            _ => {}
        }
    }

    fn seek_relative(&mut self, _c: &mut MutexGuard<'_, PlayerController>, delta_secs: i64) {
        let Some(pb) = &self.playback else { return };
        let pos_ms = pb.position_ms() as i64;
        let dur_ms = self
            .now_playing
            .as_ref()
            .map(|t| t.duration_secs as i64 * 1000)
            .unwrap_or(0);
        let target = (pos_ms + delta_secs * 1000).max(0).min(dur_ms) as u64;
        pb.request_seek(target);
        self.status = format!("seek: {}", fmt_duration((target / 1000) as u32));
    }
}

/// Play `tracks` in order: clear the queue, enqueue them, play the first.
fn play_sequence(c: &mut MutexGuard<'_, PlayerController>, tracks: Vec<Track>) {
    let mut iter = tracks.into_iter();
    if let Some(first) = iter.next() {
        c.dispatch(PlayerIntent::PlayTrack { id: first.id });
        for t in iter {
            c.dispatch(PlayerIntent::Enqueue {
                id: t.id,
                next: false,
            });
        }
    }
}

/// Enqueue `tracks` without disturbing whatever is playing now.
fn enqueue_sequence(c: &mut MutexGuard<'_, PlayerController>, tracks: Vec<Track>, next: bool) {
    for t in tracks {
        c.dispatch(PlayerIntent::Enqueue { id: t.id, next });
    }
}

/// Render a 0-5 rating as five ASCII cells, e.g. `[***  ]`.
pub fn render_rating(rating: u8) -> String {
    let r = rating.min(5) as usize;
    let mut s = String::with_capacity(7);
    s.push('[');
    for i in 0..5 {
        s.push(if i < r { '*' } else { ' ' });
    }
    s.push(']');
    s
}

/// Truncate a string to `max` chars, appending an ellipsis if it was longer.
fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}

/// Truncate a track title for display. Very long titles (e.g. artistic tags
/// like "++++++++++++++++++++++++++++++++++++++") make the line unreadable.
pub fn display_title(t: &Track) -> String {
    truncate(t.title.clone(), 50)
}

pub fn fmt_duration(secs: u32) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}
