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
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

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

/// Fixed-width column layout for the track list. Each field is padded or
/// truncated to its width. Debounced against scrolling so columns stay stable
/// while the user browses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColumnWidths {
    pub artist: usize,
    pub album: usize,
    pub title: usize, // includes track_no prefix
    pub year: usize,
    pub genre: usize,
    pub duration: usize,
}

/// Key used to detect structural vs scroll changes for column-width debouncing.
/// Structural changes (tab/view/content) adopt immediately; scroll changes
/// wait for the debounce period before adopting new widths.
type ColStructuralKey = (Tab, ViewPreset, bool, String);
type ColScrollKey = (usize, usize);

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
/// For artists, the expand goes through two levels: first albums, then
/// tracks of the selected album (`drilled = true`).
#[derive(Debug, Clone)]
pub struct ExpandedView {
    pub kind: ExpandKind,
    pub tracks: Vec<Track>,
    /// Albums of an expanded artist (empty for album kind).
    pub albums: Vec<Album>,
    /// True when showing tracks instead of the album list (artist only).
    pub drilled: bool,
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

    // --- column-width debounce state ---
    col_widths: ColumnWidths,
    col_structural: ColStructuralKey,
    col_scroll: ColScrollKey,
    col_last_scroll: Instant,
    col_initialized: bool,
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
            volume: 1.0,
            smart_playlists: Vec::new(),
            playback: None,
            pending_g: false,
            scan_progress: None,
            dirty: true,
            col_widths: ColumnWidths::default(),
            col_structural: (Tab::Tracks, ViewPreset::Compact, false, String::new()),
            col_scroll: (0, 0),
            col_last_scroll: Instant::now(),
            col_initialized: false,
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
                self.jump_to_playing(&c.store);
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

    pub fn col_widths(&self) -> ColumnWidths {
        self.col_widths
    }

    /// Recompute column widths with debounce. Call once per frame before draw,
    /// passing the full terminal width and height. Column widths are computed
    /// from only the currently visible rows (not the full 800-item window).
    pub fn update_col_widths(&mut self, full_width: usize, full_height: usize) {
        const DEBOUNCE: Duration = Duration::from_millis(1000);

        // 2 borders + 3 highlight symbol (normal), +1 extra in expanded view.
        let border_overhead = if self.expanded.is_some() { 6 } else { 5 };
        let usable_width = full_width.saturating_sub(border_overhead);

        // Layout: header(3) + list + footer(5). List area height minus 2 borders.
        let visible_rows = full_height.saturating_sub(3 + 5 + 2);

        let computed = {
            let (items, selection, offset) = if let Some(e) = &self.expanded {
                if e.drilled || e.kind == ExpandKind::Album {
                    (&e.tracks[..], e.selection, 0)
                } else {
                    (&[][..], 0, 0)
                }
            } else {
                match self.tab {
                    Tab::Tracks => (
                        &self.tracks.items[..],
                        self.tracks.selection,
                        self.tracks.offset,
                    ),
                    Tab::Files => (
                        &self.files.items[..],
                        self.files.selection,
                        self.files.offset,
                    ),
                    _ => (&[][..], 0, 0),
                }
            };

            if items.is_empty() {
                None
            } else {
                // Slice the visible rows from the window. `selection` is the
                // absolute index; `offset` is the window's first item index.
                // The on-screen position is `selection - offset` (the list_index).
                let list_index = selection.saturating_sub(offset);
                // center_select puts the selection at visible_rows/2, so the
                // visible slice starts at list_index - visible_rows/2.
                let half = visible_rows / 2;
                let vis_start = list_index.saturating_sub(half);
                let vis_end = (vis_start + visible_rows).min(items.len());
                let vis_items = &items[vis_start.min(items.len())..vis_end];

                if vis_items.is_empty() {
                    None
                } else {
                    let ideal = compute_ideal_widths(vis_items, self.view, usable_width);
                    let is_expanded = self.expanded.is_some();
                    Some((ideal, selection, offset, is_expanded))
                }
            }
        };

        match computed {
            None => {
                self.col_widths = ColumnWidths::default();
                self.col_initialized = false;
            }
            Some((ideal, selection, offset, is_expanded)) => {
                let structural = (self.tab, self.view, is_expanded, self.search.clone());
                let scroll = (selection, offset);

                if !self.col_initialized || structural != self.col_structural {
                    // First frame or structural change — adopt immediately.
                    self.col_structural = structural;
                    self.col_scroll = scroll;
                    self.col_last_scroll = Instant::now();
                    self.col_widths = ideal;
                    self.col_initialized = true;
                } else if scroll != self.col_scroll {
                    // Scroll change — debounce.
                    self.col_scroll = scroll;
                    self.col_last_scroll = Instant::now();
                } else if self.col_last_scroll.elapsed() >= DEBOUNCE {
                    self.col_widths = ideal;
                }
            }
        }
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
                KeyCode::Char('j') => self.jump_to_playing(&c.store),
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
            KeyCode::Char('q') => self.enqueue_selected(c, false),
            KeyCode::Char('n') => c.dispatch(PlayerIntent::NextTrack),
            KeyCode::Char('N') => self.enqueue_selected(c, true),
            KeyCode::Char('x') => self.remove_from_queue(c),
            KeyCode::Char('c') => {
                self.view = match self.view {
                    ViewPreset::Minimal => ViewPreset::Compact,
                    ViewPreset::Compact => ViewPreset::Full,
                    ViewPreset::Full => ViewPreset::Minimal,
                };
            }
            KeyCode::Char('s') => {
                self.cycle_sort();
                self.status = format!("sort: {}", sort_label(self.sort));
            }
            KeyCode::Char('S') => {
                c.dispatch(PlayerIntent::StopAfterCurrent);
                self.status = if c.stop_after_current {
                    "stop after current: on".into()
                } else {
                    "stop after current: off".into()
                };
            }
            KeyCode::Char('r') => self.toggle_radio(c),
            KeyCode::Char('R') => self.toggle_radio(c),
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
        if let Some(e) = &self.expanded {
            if e.drilled {
                return e.tracks.len();
            }
            return e.albums.len();
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
            if e.drilled {
                return e.tracks.get(e.selection).map(|t| t.id.clone());
            }
            // Album list level: resolve to first track of selected album.
            return e
                .albums
                .get(e.selection)
                .and_then(|a| c.store.album_tracks(&a.album).first().map(|t| t.id.clone()));
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
                Tab::Albums => self
                    .albums
                    .get(self.sel_albums)
                    .and_then(|a| c.store.album_tracks(&a.album).first().map(|t| t.id.clone())),
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
            if e.drilled {
                // Track level: play the selected individual track.
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::PlayTrack { id });
                    self.status = format!("playing: {}", e.label);
                }
            } else {
                // Album list level: play the selected album.
                if let Some(a) = e.albums.get(e.selection).cloned() {
                    let tracks = c.store.album_tracks(&a.album);
                    play_sequence(c, tracks);
                    self.status = format!("playing album: {}", a.album);
                }
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
                    let tracks = c.store.album_tracks(&a.album);
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
        if let Some(e) = &self.expanded {
            if e.drilled {
                // Track level: enqueue the selected track.
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::Enqueue { id, next });
                    self.status = if next { "play next" } else { "queued" }.into();
                }
            } else {
                // Album list level: enqueue the selected album.
                if let Some(a) = e.albums.get(e.selection).cloned() {
                    let tracks = c.store.album_tracks(&a.album);
                    enqueue_sequence(c, tracks, next);
                    self.status = if next { "album next" } else { "album queued" }.into();
                }
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
                    let tracks = c.store.album_tracks(&a.album);
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

    fn toggle_radio(&mut self, c: &mut MutexGuard<'_, PlayerController>) {
        let next = match c.dynamic_sort() {
            SortPreset::RandomAlbum => SortPreset::Random,
            _ => SortPreset::RandomAlbum,
        };
        let expr = parse_query(&self.search);
        c.set_dynamic_source(expr, next);
        self.status = format!("radio: {}", sort_label(next));
    }

    fn show_details(&mut self, c: &MutexGuard<'_, PlayerController>) {
        if let Some(id) = self.selected_track_id(c) {
            self.details = c.store.get_track(&id);
        }
    }

    fn jump_to_playing(&mut self, store: &LibraryStore) {
        let Some(np) = &self.now_playing else {
            self.status = "nothing playing".into();
            return;
        };
        let id = &np.id;
        match self.tab {
            Tab::Tracks | Tab::Files => {
                // Search within the loaded window first.
                let view = if self.tab == Tab::Tracks {
                    &self.tracks
                } else {
                    &self.files
                };
                if let Some(idx) = view.items.iter().position(|t| &t.id == id) {
                    self.set_selection(view.offset + idx);
                    self.status = "jumped to playing".into();
                    return;
                }
                // Not in the current window — query the store for its position.
                let expr = parse_query(&self.search);
                let sort = if self.tab == Tab::Tracks {
                    self.sort
                } else {
                    SortPreset::Path
                };
                if let Some(pos) = store.track_position(id, &expr, sort) {
                    self.set_selection(pos as usize);
                    self.status = "jumped to playing".into();
                } else {
                    // Track not in the filtered set or sort is random —
                    // clear the search to widen the scope and retry.
                    self.search.clear();
                    let expr = parse_query(&self.search);
                    if let Some(pos) = store.track_position(id, &expr, sort) {
                        self.invalidate_list();
                        self.set_selection(pos as usize);
                        self.status = "jumped to playing".into();
                    } else {
                        self.invalidate_list();
                        self.status = "playing track not in list".into();
                    }
                }
            }
            Tab::Albums => {
                if let Some(idx) = self.albums.iter().position(|a| a.album == np.album) {
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
        // If already expanded, advance to the next state or collapse.
        if let Some(e) = self.expanded.as_mut() {
            if e.kind == ExpandKind::Artist && !e.drilled {
                // Album list -> drill into selected album's tracks.
                if let Some(a) = e.albums.get(e.selection).cloned() {
                    let tracks = c.store.album_tracks(&a.album);
                    e.tracks = tracks;
                    e.drilled = true;
                    e.selection = 0;
                    self.status = "v: album tracks (v to collapse)".into();
                }
            } else {
                // Track level (or album kind) -> collapse.
                self.expanded = None;
                self.status.clear();
            }
            return;
        }
        // Not expanded: expand.
        match self.tab {
            Tab::Albums => {
                if let Some(a) = self.albums.get(self.sel_albums).cloned() {
                    let tracks = c.store.album_tracks(&a.album);
                    let label = format!("{} - {}", a.artist, a.album);
                    self.expanded = Some(ExpandedView {
                        kind: ExpandKind::Album,
                        tracks,
                        albums: Vec::new(),
                        drilled: true,
                        selection: 0,
                        label,
                    });
                    self.status = "v: expanded album (v to collapse)".into();
                }
            }
            Tab::Artists => {
                if let Some(a) = self.artists.get(self.sel_artists).cloned() {
                    let albums = c.store.artist_albums(&a.name);
                    let label = a.name.clone();
                    self.expanded = Some(ExpandedView {
                        kind: ExpandKind::Artist,
                        tracks: Vec::new(),
                        albums,
                        drilled: false,
                        selection: 0,
                        label,
                    });
                    self.status = "v: artist albums (v: expand album, Enter: play)".into();
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

/// Display width of a string (accounts for wide CJK characters).
pub fn disp_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate a string to `max` display columns, appending an ellipsis if it
/// was wider. Handles wide (CJK) characters that take 2 columns each.
pub fn truncate(s: String, max: usize) -> String {
    if max == 0 {
        String::new()
    } else if s.width() <= max {
        s
    } else {
        let mut out = String::new();
        let mut w = 0usize;
        for c in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > max - 1 {
                break;
            }
            out.push(c);
            w += cw;
        }
        format!("{out}…")
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

pub fn sort_label(sort: SortPreset) -> &'static str {
    match sort {
        SortPreset::ArtistAlbumTrack => "artist/album",
        SortPreset::YearDesc => "year",
        SortPreset::MostPlayed => "most played",
        SortPreset::HighestRated => "highest rated",
        SortPreset::Random => "random",
        SortPreset::RandomAlbum => "random album",
        SortPreset::Path => "path",
    }
}

// --- column width computation ---

/// Compute ideal column widths for the given tracks and view preset.
///
/// Fixed fields (year, duration) keep their natural max width. Flex fields
/// (artist, album, title, genre) keep their natural width when everything
/// fits; when it overflows, remaining space is distributed by ratio
/// (artist:album:title:genre = 3:3:3:1), capped at natural width with
/// surplus redistributed to fields that need more.
pub fn compute_ideal_widths(
    items: &[Track],
    view: ViewPreset,
    usable_width: usize,
) -> ColumnWidths {
    // Natural-width caps so one very long value in the window doesn't starve
    // the other columns. Title is already capped at 50 by display_title.
    const ARTIST_CAP: usize = 25;
    const ALBUM_CAP: usize = 35;
    const GENRE_CAP: usize = 15;

    let mut max_artist = 0usize;
    let mut max_album = 0usize;
    let mut max_title = 0usize;
    let mut max_year = 0usize;
    let mut max_genre = 0usize;
    let mut max_duration = 0usize;

    for t in items {
        max_artist = max_artist.max(disp_width(&t.artist)).min(ARTIST_CAP);
        if view != ViewPreset::Minimal {
            max_album = max_album.max(disp_width(&t.album)).min(ALBUM_CAP);
        }
        let track_no_len = if t.track_number > 0 {
            disp_width(&format!("{:02}. ", t.track_number))
        } else {
            0
        };
        max_title = max_title.max(track_no_len + disp_width(&display_title(t)));
        if view == ViewPreset::Full {
            if t.year > 0 {
                max_year = max_year.max(disp_width(&t.year.to_string()));
            }
            max_genre = max_genre.max(disp_width(&t.genre)).min(GENRE_CAP);
        }
        max_duration = max_duration.max(disp_width(&fmt_duration(t.duration_secs)));
    }

    // marker(2) + rating(8: space + "[*****]")
    let available = usable_width.saturating_sub(2 + 8);

    match view {
        ViewPreset::Minimal => {
            // columns: artist, title, duration (3 cols, 2 separators)
            let flex = resolve_flex(
                &[max_artist, max_title],
                &[3, 3],
                max_duration,
                2,
                available,
            );
            ColumnWidths {
                artist: flex[0],
                album: 0,
                title: flex[1],
                year: 0,
                genre: 0,
                duration: max_duration,
            }
        }
        ViewPreset::Compact => {
            // columns: artist, album, title, duration (4 cols, 3 separators)
            let flex = resolve_flex(
                &[max_artist, max_album, max_title],
                &[3, 3, 3],
                max_duration,
                3,
                available,
            );
            ColumnWidths {
                artist: flex[0],
                album: flex[1],
                title: flex[2],
                year: 0,
                genre: 0,
                duration: max_duration,
            }
        }
        ViewPreset::Full => {
            // columns: artist, album, title, year, genre, duration (6 cols, 5 separators)
            let fixed = max_year + max_duration;
            let flex = resolve_flex(
                &[max_artist, max_album, max_title, max_genre],
                &[3, 3, 3, 1],
                fixed,
                5,
                available,
            );
            ColumnWidths {
                artist: flex[0],
                album: flex[1],
                title: flex[2],
                year: max_year,
                genre: flex[3],
                duration: max_duration,
            }
        }
    }
}

/// Resolve flex column widths: use natural widths if they fit, otherwise
/// distribute remaining space by ratio.
fn resolve_flex(
    naturals: &[usize],
    ratios: &[usize],
    fixed: usize,
    separators: usize,
    available: usize,
) -> Vec<usize> {
    let total_natural = fixed + naturals.iter().sum::<usize>() + separators;
    if total_natural <= available {
        return naturals.to_vec();
    }
    let remaining = available.saturating_sub(fixed + separators);
    distribute_flex(naturals, ratios, remaining)
}

/// Distribute `remaining` chars among flex fields by ratio, capped at each
/// field's natural width. Surplus from fields that need less is redistributed
/// to fields that still need more, prioritizing those with the largest
/// deficit. This minimizes truncation across all columns.
fn distribute_flex(naturals: &[usize], ratios: &[usize], remaining: usize) -> Vec<usize> {
    let n = naturals.len();
    if n == 0 {
        return vec![];
    }
    if remaining == 0 {
        return vec![0; n];
    }
    let total_ratio: usize = ratios.iter().sum::<usize>().max(1);

    let mut widths: Vec<usize> = (0..n)
        .map(|i| (remaining * ratios[i]) / total_ratio)
        .collect();

    // Cap at natural, collect surplus.
    let mut surplus = 0usize;
    for i in 0..n {
        if widths[i] > naturals[i] {
            surplus += widths[i] - naturals[i];
            widths[i] = naturals[i];
        }
    }

    // Redistribute surplus one unit at a time to the field with the largest
    // deficit (natural - current), until no field needs more or surplus runs
    // out. This greedily minimizes the maximum truncation ratio.
    while surplus > 0 {
        let best = (0..n)
            .filter(|&i| widths[i] < naturals[i])
            .max_by_key(|&i| naturals[i] - widths[i]);
        match best {
            Some(i) => {
                widths[i] += 1;
                surplus -= 1;
            }
            None => break,
        }
    }

    widths
}

/// Left-align text in a fixed-width column: truncate with ellipsis if too
/// wide, right-pad with spaces if too narrow. Uses display width so wide
/// (CJK) characters are handled correctly.
pub fn col_text(s: &str, width: usize) -> String {
    let len = disp_width(s);
    if len > width {
        truncate(s.to_string(), width)
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

/// Right-align a value in a fixed-width column: left-pad with spaces.
pub fn col_num(s: &str, width: usize) -> String {
    let len = disp_width(s);
    if len > width {
        // Truncate by display width from the left.
        let mut out = String::new();
        let mut w = 0usize;
        for c in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > width {
                break;
            }
            out.push(c);
            w += cw;
        }
        out
    } else {
        format!("{}{s}", " ".repeat(width - len))
    }
}
