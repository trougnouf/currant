// ./tui/src/app.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! TUI application state: tabs, search, sort, view presets, windowed caches
//! and the key bindings that drive the controller.

use framboise_core::controller::PlayerController;
use framboise_core::matcher::{self, parse_query};
use framboise_core::model::{Album, Artist, PlayerIntent, SortPreset, Track};
use framboise_core::scanner::ScanProgress;
use framboise_core::store::LibraryStore;
use std::sync::{Arc, MutexGuard};

/// Library view tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tracks,
    Albums,
    Artists,
    Queue,
    Files,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Tracks,
        Tab::Albums,
        Tab::Artists,
        Tab::Queue,
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
        self.selection = 0;
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

    tracks: WindowedView,
    files: WindowedView,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    sel_albums: usize,
    sel_artists: usize,
    sel_queue: usize,
    queue_rows: Vec<QueueRow>,

    // view model rebuilt each refresh
    pub now_playing: Option<Track>,
    pub is_playing: bool,
    pub stop_after: bool,
    pub track_count: u64,
    pub radio_sort: Option<SortPreset>,

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
            tracks: WindowedView::new(),
            files: WindowedView::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            sel_albums: 0,
            sel_artists: 0,
            sel_queue: 0,
            queue_rows: Vec::new(),
            now_playing: None,
            is_playing: false,
            stop_after: false,
            track_count: 0,
            radio_sort: None,
            scan_progress: None,
            dirty: true,
        }
    }

    pub fn set_scan_progress(&mut self, p: Arc<ScanProgress>) {
        self.scan_progress = Some(p);
    }

    /// Rebuild caches and the view model from the controller. Called each frame.
    pub fn refresh(&mut self, c: &MutexGuard<'_, PlayerController>) {
        let expr = parse_query(&self.search);
        self.now_playing = c.current_track_ref();
        self.is_playing = c.is_playing;
        self.stop_after = c.stop_after_current;
        self.track_count = c.store.track_count();
        self.radio_sort = Some(c.dynamic_sort());

        // While a scan is running, keep the lists refreshing so new tracks
        // appear live, and show progress in the status bar.
        if let Some(p) = &self.scan_progress {
            if !p.is_done() {
                self.dirty = true;
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
            }
        }

        match self.tab {
            Tab::Tracks => self.refresh_tracks(&c.store, &expr),
            Tab::Files => self.refresh_files(&c.store, &expr),
            Tab::Albums => self.refresh_albums(&c.store, &expr),
            Tab::Artists => self.refresh_artists(&c.store, &expr),
            Tab::Queue => self.refresh_queue(c),
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

    // --- input ---

    /// Handle a key. Returns true to quit. `c` is the locked controller.
    pub fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        c: &mut MutexGuard<'_, PlayerController>,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

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
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.prev(),
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
            _ => {}
        }
        false
    }

    fn invalidate_list(&mut self) {
        self.dirty = true;
    }

    fn reset_selection(&mut self) {
        match self.tab {
            Tab::Tracks => self.tracks.selection = 0,
            Tab::Files => self.files.selection = 0,
            Tab::Albums => self.sel_albums = 0,
            Tab::Artists => self.sel_artists = 0,
            Tab::Queue => self.sel_queue = 0,
        }
    }

    fn current_total(&self) -> usize {
        match self.tab {
            Tab::Tracks => self.tracks.total as usize,
            Tab::Files => self.files.total as usize,
            Tab::Albums => self.albums.len(),
            Tab::Artists => self.artists.len(),
            Tab::Queue => self.queue_rows.len(),
        }
    }

    fn current_selection(&self) -> usize {
        match self.tab {
            Tab::Tracks => self.tracks.selection,
            Tab::Files => self.files.selection,
            Tab::Albums => self.sel_albums,
            Tab::Artists => self.sel_artists,
            Tab::Queue => self.sel_queue,
        }
    }

    fn set_selection(&mut self, i: usize) {
        let max = self.current_total().saturating_sub(1);
        let i = i.min(max);
        match self.tab {
            Tab::Tracks => self.tracks.selection = i,
            Tab::Files => self.files.selection = i,
            Tab::Albums => self.sel_albums = i,
            Tab::Artists => self.sel_artists = i,
            Tab::Queue => self.sel_queue = i,
        }
        // Force the window to reload if we scrolled off it.
        if matches!(self.tab, Tab::Tracks | Tab::Files) {
            self.dirty = true;
        }
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
            Tab::Albums | Tab::Artists => None,
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
        match self.tab {
            Tab::Tracks | Tab::Files | Tab::Queue => {
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::PlayTrack { id });
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
        }
    }

    fn enqueue_selected(&mut self, c: &mut MutexGuard<'_, PlayerController>, next: bool) {
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
            Tab::Queue => {}
        }
    }

    fn remove_from_queue(&mut self, c: &mut MutexGuard<'_, PlayerController>) {
        if self.tab != Tab::Queue {
            return;
        }
        if let Some(row) = self.queue_rows.get(self.sel_queue).cloned() {
            c.dispatch(PlayerIntent::RemoveFromQueue { id: row.id });
            self.status = "removed from queue".into();
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

pub fn fmt_duration(secs: u32) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}
