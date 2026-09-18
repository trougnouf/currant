// ./tui/src/app.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! TUI application state: tabs, search, sort, view presets, windowed caches
//! and the key bindings that drive the controller.

use crate::audio::PlaybackState;
use currant_core::controller::PlayerController;
use currant_core::matcher::{self, parse_query};
use currant_core::model::{Album, Artist, PlayerIntent, SmartPlaylist, SortPreset, Track};
use currant_core::scanner::{ScanProgress, default_roots, scan_roots};
use currant_core::scrobble::{ListenbrainzScrobbler, NoopScrobbler};
use currant_core::store::LibraryStore;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
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

/// One editable row in the settings pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    /// A configured scan root (index into `SettingsPane::roots`).
    Root(usize),
    /// The "+ add root" row.
    AddRoot,
    /// The Listenbrainz scrobble token.
    Token,
    /// The default volume (0-100).
    Volume,
    /// Apply ReplayGain tags.
    ReplayGain,
    /// Watch roots for changes.
    WatchRoots,
    /// Update column widths live while scrolling (no debounce).
    LiveColumns,
}

/// The settings overlay: scan roots, scrobble token, default volume, watch roots, replaygain, and live columns.
/// Opened with `o`; `Esc` saves and closes, `Enter` edits the selected row.
#[derive(Debug)]
pub struct SettingsPane {
    pub roots: Vec<String>,
    /// Roots as they were when the pane opened, to detect changes on save.
    orig_roots: Vec<String>,
    pub token: String,
    pub volume: f32,
    pub replaygain: bool,
    pub watch_roots: bool,
    pub live_columns: bool,
    pub selected: usize,
    pub editing: Option<SettingsField>,
    pub edit_buf: String,
}

impl SettingsPane {
    pub fn new(
        roots: Vec<String>,
        token: String,
        volume: f32,
        replaygain: bool,
        watch_roots: bool,
        live_columns: bool,
    ) -> Self {
        Self {
            orig_roots: roots.clone(),
            roots,
            token,
            volume,
            replaygain,
            watch_roots,
            live_columns,
            selected: 0,
            editing: None,
            edit_buf: String::new(),
        }
    }

    /// True if the roots differ from when the pane was opened.
    pub fn roots_changed(&self) -> bool {
        self.roots != self.orig_roots
    }

    /// Total number of rows: one per root, plus add-root, token, volume, replaygain, watch roots, live columns.
    pub fn field_count(&self) -> usize {
        self.roots.len() + 6
    }

    /// The field at row `i`, if any.
    pub fn field_at(&self, i: usize) -> Option<SettingsField> {
        match i {
            i if i < self.roots.len() => Some(SettingsField::Root(i)),
            i if i == self.roots.len() => Some(SettingsField::AddRoot),
            i if i == self.roots.len() + 1 => Some(SettingsField::Token),
            i if i == self.roots.len() + 2 => Some(SettingsField::Volume),
            i if i == self.roots.len() + 3 => Some(SettingsField::ReplayGain),
            i if i == self.roots.len() + 4 => Some(SettingsField::WatchRoots),
            i if i == self.roots.len() + 5 => Some(SettingsField::LiveColumns),
            _ => None,
        }
    }

    /// (label, value) text for a row, for rendering.
    pub fn field_text(&self, field: &SettingsField) -> (&str, String) {
        match field {
            SettingsField::Root(i) => {
                let path = self.roots.get(*i).cloned().unwrap_or_default();
                ("scan root", path)
            }
            SettingsField::AddRoot => ("scan root", "+ add".to_string()),
            SettingsField::Token => ("listenbrainz token", self.token.clone()),
            SettingsField::Volume => (
                "default volume",
                format!("{:.0}%", (self.volume * 100.0).round()),
            ),
            SettingsField::ReplayGain => (
                "replaygain",
                if self.replaygain {
                    "enabled".into()
                } else {
                    "disabled".into()
                },
            ),
            SettingsField::WatchRoots => (
                "watch roots",
                if self.watch_roots {
                    "enabled".into()
                } else {
                    "disabled".into()
                },
            ),
            SettingsField::LiveColumns => (
                "live columns",
                if self.live_columns {
                    "enabled".into()
                } else {
                    "disabled".into()
                },
            ),
        }
    }

    /// Begin editing `field`, seeding the buffer with its current value.
    pub fn begin_edit(&mut self, field: SettingsField) {
        let seed = match field {
            SettingsField::Root(i) => self.roots.get(i).cloned().unwrap_or_default(),
            SettingsField::AddRoot => String::new(),
            SettingsField::Token => self.token.clone(),
            SettingsField::Volume => format!("{:.0}", (self.volume * 100.0).round()),
            SettingsField::ReplayGain | SettingsField::WatchRoots | SettingsField::LiveColumns => {
                String::new()
            }
        };
        self.editing = Some(field);
        self.edit_buf = seed;
    }

    /// Commit the edit buffer to the field being edited.
    pub fn commit_edit(&mut self) {
        match self.editing {
            Some(SettingsField::Root(i)) => {
                let buf = self.edit_buf.trim().to_string();
                if buf.is_empty() {
                    self.roots.remove(i);
                } else {
                    self.roots[i] = buf;
                }
            }
            Some(SettingsField::AddRoot) => {
                let buf = self.edit_buf.trim().to_string();
                if !buf.is_empty() {
                    self.roots.push(buf);
                }
            }
            Some(SettingsField::Token) => self.token = self.edit_buf.trim().to_string(),
            Some(SettingsField::Volume) => {
                if let Ok(v) = self.edit_buf.trim().parse::<f32>() {
                    self.volume = (v / 100.0).clamp(0.0, 1.0);
                }
            }
            // Bool rows are toggled directly, never committed from the buffer.
            Some(SettingsField::ReplayGain)
            | Some(SettingsField::WatchRoots)
            | Some(SettingsField::LiveColumns) => {}
            None => {}
        }
        self.editing = None;
        self.edit_buf.clear();
        // Keep the selection in range after a root is removed.
        self.selected = self.selected.min(self.field_count().saturating_sub(1));
    }

    /// Remove the root at `i`, keeping the selection in range.
    pub fn remove_root(&mut self, i: usize) {
        if i < self.roots.len() {
            self.roots.remove(i);
            self.selected = i.min(self.field_count().saturating_sub(1));
        }
    }
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
    pub help_scroll: u16,
    pub help_lines: usize,
    pub help_height: u16,
    pub details: Option<Track>,
    pub settings: Option<SettingsPane>,

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
    pub stop_after: Option<String>,
    pub track_count: u64,
    pub radio_sort: Option<SortPreset>,
    pub volume: f32,
    pub smart_playlists: Vec<SmartPlaylist>,

    /// Shared playback state (position + seek channel) with the audio thread.
    playback: Option<Arc<PlaybackState>>,

    /// True when `g` was pressed and we expect a digit to activate a playlist.
    pending_g: bool,

    /// Last mouse click (time, item index) for double-click detection.
    last_mouse_click: Option<(Instant, usize)>,

    /// Live scan progress; `None` when no scan is running.
    scan_progress: Option<Arc<ScanProgress>>,

    pub watcher_tx: Option<std::sync::mpsc::Sender<Vec<String>>>,
    pub watcher_progress_rx: Option<std::sync::mpsc::Receiver<Arc<ScanProgress>>>,

    pub network_state: Option<Arc<std::sync::Mutex<currant_core::net::NetworkState>>>,

    /// Sender for the zone (remote-control) client thread. The UI uses it to
    /// switch the active zone or forward intents to a remote Playback Target.
    pub zone_tx: Option<std::sync::mpsc::Sender<crate::zone::ZoneCommand>>,

    dirty: bool,

    /// ListState offset for the currently visible list (relative to the
    /// items array, not the catalog). When `follow` is true the renderer
    /// re-centers on the selection every frame; when false it keeps this
    /// offset, adjusting only if the selection leaves the viewport.
    scroll_offset: usize,
    follow: bool,

    // --- column-width debounce state ---
    col_widths: ColumnWidths,
    col_structural: ColStructuralKey,
    col_scroll: ColScrollKey,
    col_last_scroll: Instant,
    col_initialized: bool,
    /// When true, column widths update immediately on scroll (no debounce).
    pub live_columns: bool,
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
            help_scroll: 0,
            help_lines: 0,
            help_height: 0,
            details: None,
            settings: None,
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
            stop_after: None,
            track_count: 0,
            radio_sort: None,
            volume: 1.0,
            smart_playlists: Vec::new(),
            playback: None,
            pending_g: false,
            last_mouse_click: None,
            scan_progress: None,
            dirty: true,
            scroll_offset: 0,
            follow: true,
            col_widths: ColumnWidths::default(),
            col_structural: (Tab::Tracks, ViewPreset::Compact, false, String::new()),
            col_scroll: (0, 0),
            col_last_scroll: Instant::now(),
            col_initialized: false,
            live_columns: false,
            watcher_tx: None,
            watcher_progress_rx: None,
            network_state: None,
            zone_tx: None,
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
        if let Some(rx) = &self.watcher_progress_rx
            && let Ok(p) = rx.try_recv()
        {
            self.scan_progress = Some(p);
        }

        let expr = parse_query(&self.search);
        self.now_playing = c.current_track_ref();
        self.is_playing = c.is_playing;
        self.stop_after = c.stop_after.clone();
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
            self.scroll_offset = 0;
            self.follow = true;
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

    /// Current ListState offset (relative to the items array) for the renderer.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// (selected, items_len) matching what the render path passes to the ListState.
    fn list_state_dims(&self) -> (usize, usize) {
        if let Some(e) = &self.expanded {
            if e.drilled || e.kind == ExpandKind::Album {
                return (e.selection, e.tracks.len());
            }
            return (e.selection, e.albums.len());
        }
        match self.tab {
            Tab::Tracks => (self.tracks.list_index(), self.tracks.items.len()),
            Tab::Files => (self.files.list_index(), self.files.items.len()),
            Tab::Albums => (self.sel_albums, self.albums.len()),
            Tab::Artists => (self.sel_artists, self.artists.len()),
            Tab::Queue => (self.sel_queue, self.queue_rows.len()),
            Tab::Playlists => (self.sel_playlists, self.smart_playlists.len()),
        }
    }

    /// Absolute index (in the catalog) of the first visible row, for click hit-testing.
    fn absolute_scroll_offset(&self) -> usize {
        if self.expanded.is_some() {
            return self.scroll_offset;
        }
        match self.tab {
            Tab::Tracks => self.tracks.offset.saturating_add(self.scroll_offset),
            Tab::Files => self.files.offset.saturating_add(self.scroll_offset),
            _ => self.scroll_offset,
        }
    }

    /// Reconcile `scroll_offset` with the current selection. When `follow` is
    /// true, center the selection; otherwise keep the offset and only adjust
    /// if the selection has left the viewport. Called once per frame from `draw`.
    pub fn sync_scroll(&mut self, area_height: u16) {
        let visible = area_height.saturating_sub(2) as usize; // borders
        if visible == 0 {
            return;
        }
        let (selected, total) = self.list_state_dims();
        if total == 0 {
            self.scroll_offset = 0;
            return;
        }
        let max_offset = total.saturating_sub(visible);
        if self.follow {
            let half = visible / 2;
            self.scroll_offset = selected.saturating_sub(half).min(max_offset);
        } else {
            let offset = self.scroll_offset.min(max_offset);
            if selected < offset {
                self.scroll_offset = selected;
            } else if selected >= offset + visible {
                self.scroll_offset = selected
                    .saturating_sub(visible.saturating_sub(1))
                    .min(max_offset);
            } else {
                self.scroll_offset = offset;
            }
        }
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
                // Slice the visible rows from the window using the scroll
                // offset (relative to the items array).
                let vis_start = self.scroll_offset;
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
                    // Scroll change — debounce, or adopt immediately if live
                    // columns are enabled.
                    self.col_scroll = scroll;
                    self.col_last_scroll = Instant::now();
                    if self.live_columns {
                        self.col_widths = ideal;
                    }
                } else if self.col_last_scroll.elapsed() >= DEBOUNCE {
                    self.col_widths = ideal;
                }
            }
        }
    }

    // --- input ---

    /// Switch to a tab, resetting expansion and marking the view dirty.
    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.expanded = None;
        self.scroll_offset = 0;
        self.follow = true;
        self.dirty = true;
    }

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

        // Settings pane: capture its own keys (Esc saves and closes).
        if self.settings.is_some() {
            // Ctrl+C still quits, so the pane can never trap the user.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return true;
            }
            self.handle_settings_key(key, c);
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

        // Help overlay: scroll or close.
        if self.help {
            let max = self.help_lines.saturating_sub(self.help_height as usize) as u16;
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => self.help = false,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(max);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(10).min(max);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                }
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = max,
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
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                if self.help {
                    self.help = false;
                }
            }
            KeyCode::Tab => self.switch_tab(self.tab.next()),
            KeyCode::BackTab => self.switch_tab(self.tab.prev()),
            KeyCode::F(1) => self.switch_tab(Tab::Tracks),
            KeyCode::F(2) => self.switch_tab(Tab::Albums),
            KeyCode::F(3) => self.switch_tab(Tab::Artists),
            KeyCode::F(4) => self.switch_tab(Tab::Queue),
            KeyCode::F(5) => self.switch_tab(Tab::Playlists),
            KeyCode::F(6) => self.switch_tab(Tab::Files),
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
            KeyCode::Char('N') => c.dispatch(PlayerIntent::SkipAlbum),
            KeyCode::Char('e') => self.enqueue_selected(c, false),
            KeyCode::Char('n') => c.dispatch(PlayerIntent::NextTrack),
            KeyCode::Char('f') => self.enqueue_selected(c, true),
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
                let id = self.selected_track_id(c).unwrap_or_default();
                c.dispatch(PlayerIntent::StopAfter { id });
                self.status = match &c.stop_after {
                    Some(sid) => {
                        let label = c
                            .store
                            .get_track(sid)
                            .map(|t| format!("{} - {}", t.artist, t.title))
                            .unwrap_or_else(|| sid.clone());
                        format!("stop after: {label}")
                    }
                    None => "stop after: off".into(),
                };
            }
            KeyCode::Char('r') => self.toggle_radio(c),
            KeyCode::Char('R') => self.toggle_radio(c),
            KeyCode::Char('?') => {
                self.help = !self.help;
                self.help_scroll = 0;
            }
            KeyCode::Char('d') => self.show_details(c),
            KeyCode::Char('o') => self.open_settings(c),
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
            KeyCode::Char('J') => self.move_playlist(c, false),
            KeyCode::Char('K') => self.move_playlist(c, true),
            _ => {}
        }
        false
    }

    /// Handle a mouse event. `size` is the current terminal rect, used for
    /// hit-testing against the same layout the renderer computes.
    pub fn handle_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        c: &mut MutexGuard<'_, PlayerController>,
        size: Rect,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};

        // Settings pane: keyboard-driven, ignore mouse.
        if self.settings.is_some() {
            return false;
        }

        // Overlays: any left-click closes them (same as key behavior).
        if self.details.is_some() && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.details = None;
            return false;
        }
        if self.help {
            let max = self.help_lines.saturating_sub(self.help_height as usize) as u16;
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => self.help = false,
                MouseEventKind::ScrollDown => {
                    self.help_scroll = self.help_scroll.saturating_add(3).min(max);
                }
                MouseEventKind::ScrollUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(3);
                }
                _ => {}
            }
            return false;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // tabs + search
                Constraint::Min(0),    // list
                Constraint::Length(5), // now playing + progress + status
            ])
            .split(size);

        let col = mouse.column;
        let row = mouse.row;

        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if row < chunks[0].bottom() {
                    self.handle_tab_click(col, row, chunks[0]);
                } else if row >= chunks[1].top() && row < chunks[1].bottom() {
                    self.handle_list_click(row, chunks[1], c);
                } else if row >= chunks[2].top() && row < chunks[2].bottom() {
                    self.handle_footer_click(col, row, chunks[2]);
                }
            }
            _ => {}
        }
        false
    }

    /// Click on a tab title in the header area.
    fn handle_tab_click(&mut self, col: u16, row: u16, header_area: Rect) {
        // Tabs occupy the first 60 columns of the header.
        let header_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(60), Constraint::Min(0)])
            .split(header_area);
        let tab_area = header_cols[0];

        // Tab titles are on the first inner line of the block (below top border).
        if row != tab_area.y + 1 {
            return;
        }

        let inner_x = (tab_area.x + 1) as usize; // left border
        let titles = ["tracks", "albums", "artists", "queue", "playlists", "files"];
        let mut x = inner_x;
        for (i, title) in titles.iter().enumerate() {
            let w = title.len();
            if col as usize >= x && (col as usize) < x + w {
                self.tab = Tab::ALL[i];
                self.expanded = None;
                self.scroll_offset = 0;
                self.follow = true;
                self.dirty = true;
                return;
            }
            x += w;
        }
    }

    /// Click on a row in the list area: select the item, or activate on
    /// double-click.
    fn handle_list_click(
        &mut self,
        row: u16,
        area: Rect,
        c: &mut MutexGuard<'_, PlayerController>,
    ) {
        // Must be inside the top and bottom borders.
        if row <= area.top() || row >= area.bottom() {
            return;
        }
        let content_row = (row - area.top() - 1) as usize;
        let visible = area.height.saturating_sub(2) as usize; // borders
        let total = self.current_total();
        if total == 0 || visible == 0 {
            return;
        }

        // Use the renderer's scroll offset so the row-to-item mapping is
        // consistent with what's on screen.
        let (_, items_len) = self.list_state_dims();
        let local_row = self.scroll_offset + content_row;
        if local_row >= items_len {
            return;
        }
        let item_index = self.absolute_scroll_offset() + content_row;
        if item_index >= total {
            return;
        }

        self.follow = false; // keep the view in place on click
        self.set_selection(item_index);

        // Double-click activates (play) the item.
        let now = Instant::now();
        if let Some((t, idx)) = self.last_mouse_click
            && idx == item_index
            && now.duration_since(t) < Duration::from_millis(400)
        {
            self.activate(c);
            self.last_mouse_click = None;
            return;
        }
        self.last_mouse_click = Some((now, item_index));
    }

    /// Click on the progress bar in the footer: seek to the clicked position.
    fn handle_footer_click(&mut self, col: u16, row: u16, area: Rect) {
        // Footer: 4-row now-playing block + 1-row status.
        let footer_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Length(1)])
            .split(area);

        // Gauge is on the second inner line of the now-playing block.
        let np_inner = footer_chunks[0].inner(Margin::new(1, 1));
        let gauge_row = np_inner.y + 1;
        if row != gauge_row {
            return;
        }

        let Some(pb) = &self.playback else { return };
        let Some(t) = &self.now_playing else { return };
        if t.duration_secs == 0 {
            return;
        }

        let gauge_w = np_inner.width as usize;
        if gauge_w == 0 {
            return;
        }
        let pos = (col as usize)
            .saturating_sub(np_inner.x as usize)
            .min(gauge_w - 1);
        let fraction = pos as f64 / gauge_w as f64;
        let target_ms = (fraction * t.duration_secs as f64 * 1000.0) as u64;
        pb.request_seek(target_ms);
        self.status = format!("seek: {}", fmt_duration((target_ms / 1000) as u32));
    }

    fn invalidate_list(&mut self) {
        self.dirty = true;
    }

    fn reset_selection(&mut self) {
        self.scroll_offset = 0;
        self.follow = true;
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
        self.follow = true;
        let cur = self.current_selection() as i64;
        let next = cur.saturating_add(delta).max(0) as usize;
        self.set_selection(next);
    }

    fn move_selection_to(&mut self, i: usize) {
        self.follow = true;
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
            return e.albums.get(e.selection).and_then(|a| {
                c.store
                    .album_tracks(&a.artist, &a.album)
                    .first()
                    .map(|t| t.id.clone())
            });
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
            if e.drilled {
                // Track level: play the selected individual track.
                if let Some(id) = self.selected_track_id(c) {
                    c.dispatch(PlayerIntent::PlayTrack { id });
                    self.status = format!("playing: {}", e.label);
                }
            } else {
                // Album list level: play the selected album.
                if let Some(a) = e.albums.get(e.selection).cloned() {
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
                    play_sequence(c, tracks);
                    self.status = format!("playing album: {}", a.album);
                }
            }
            return;
        }
        match self.tab {
            Tab::Tracks | Tab::Files => {
                let list_sort = if self.tab == Tab::Tracks {
                    self.sort
                } else {
                    SortPreset::Path
                };
                let expr = parse_query(&self.search);
                let radio_sort = c.dynamic_sort();
                let is_radio = matches!(radio_sort, SortPreset::Random | SortPreset::RandomAlbum);

                if let Some(id) = self.selected_track_id(c) {
                    if is_radio {
                        // Radio: play just this track; the dynamic queue
                        // (constrained to the filter) handles next.
                        c.set_dynamic_source(expr, radio_sort);
                        c.dispatch(PlayerIntent::PlayTrack { id });
                    } else if let Some(pos) = c.store.track_position(&id, &expr, list_sort) {
                        // Ordered: play the rest of the filtered list from here.
                        let ids = c.store.filter_ids(&expr, list_sort, u32::MAX, pos as u32);
                        c.set_dynamic_source(expr, radio_sort);
                        play_sequence_ids(c, ids);
                    } else {
                        // Random list sort has no stable position — play just
                        // this track. No dynamic refill in ordered mode, so
                        // Next stops after this track.
                        c.set_dynamic_source(expr, radio_sort);
                        c.dispatch(PlayerIntent::PlayTrack { id });
                    }
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
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
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

    fn move_playlist(&mut self, c: &mut MutexGuard<'_, PlayerController>, up: bool) {
        if self.tab != Tab::Playlists {
            return;
        }
        if let Some(pl) = self.smart_playlists.get(self.sel_playlists).cloned() {
            c.dispatch(PlayerIntent::MovePlaylist {
                id: pl.id.clone(),
                up,
            });
            if up && self.sel_playlists > 0 {
                self.sel_playlists -= 1;
            } else if !up && self.sel_playlists + 1 < self.smart_playlists.len() {
                self.sel_playlists += 1;
            }
            self.status = format!(
                "moved playlist {}: {}",
                if up { "up" } else { "down" },
                pl.name
            );
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
            SortPreset::Random => {
                // Turn radio off — use the list sort, but never a random one.
                if matches!(self.sort, SortPreset::Random | SortPreset::RandomAlbum) {
                    SortPreset::ArtistAlbumTrack
                } else {
                    self.sort
                }
            }
            _ => SortPreset::RandomAlbum,
        };
        let expr = parse_query(&self.search);
        c.set_dynamic_source(expr, next);
        self.status = if matches!(next, SortPreset::Random | SortPreset::RandomAlbum) {
            format!("radio: {}", sort_label(next))
        } else {
            "radio: off".into()
        };
    }

    fn show_details(&mut self, c: &MutexGuard<'_, PlayerController>) {
        if let Some(id) = self.selected_track_id(c) {
            self.details = c.store.get_track(&id);
        }
    }

    /// Open the settings pane, seeded from the store and current volume.
    fn open_settings(&mut self, c: &MutexGuard<'_, PlayerController>) {
        // Fall back to the default roots so the pane shows what is actually
        // scanned when none have been saved yet.
        let roots = c.store.load_roots();
        let roots = if roots.is_empty() {
            default_roots()
                .into_iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        } else {
            roots
        };
        let token = c.store.load_scrobble_token();
        let replaygain = c.store.load_replaygain();
        let watch_roots = c.store.load_watch_roots();
        let live_columns = c.store.load_live_columns();
        self.settings = Some(SettingsPane::new(
            roots,
            token,
            c.volume,
            replaygain,
            watch_roots,
            live_columns,
        ));
        self.status = "settings: Esc saves & closes, Space/Enter toggles bools".into();
    }

    /// Handle a key while the settings pane is open.
    fn handle_settings_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        c: &mut MutexGuard<'_, PlayerController>,
    ) {
        use crossterm::event::KeyCode;

        // Editing mode: capture input into the buffer.
        if self.settings.as_ref().is_some_and(|p| p.editing.is_some()) {
            match key.code {
                KeyCode::Esc => {
                    if let Some(p) = self.settings.as_mut() {
                        p.editing = None;
                        p.edit_buf.clear();
                    }
                }
                KeyCode::Enter => {
                    if let Some(p) = self.settings.as_mut() {
                        p.commit_edit();
                    }
                }
                KeyCode::Backspace => {
                    if let Some(p) = self.settings.as_mut() {
                        p.edit_buf.pop();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(p) = self.settings.as_mut() {
                        p.edit_buf.push(ch);
                    }
                }
                _ => {}
            }
            return;
        }

        // List mode: navigate and edit.
        match key.code {
            KeyCode::Esc => self.save_settings(c),
            KeyCode::Up => {
                if let Some(p) = self.settings.as_mut() {
                    p.selected = p.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(p) = self.settings.as_mut() {
                    p.selected = (p.selected + 1).min(p.field_count().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(p) = self.settings.as_mut()
                    && let Some(field) = p.field_at(p.selected)
                {
                    match field {
                        SettingsField::ReplayGain => p.replaygain = !p.replaygain,
                        SettingsField::WatchRoots => p.watch_roots = !p.watch_roots,
                        SettingsField::LiveColumns => p.live_columns = !p.live_columns,
                        _ => {
                            if key.code == KeyCode::Enter {
                                p.begin_edit(field);
                            }
                        }
                    }
                }
            }
            // Remove the selected scan root (no-op on the other rows).
            KeyCode::Char('x') => {
                if let Some(p) = self.settings.as_mut()
                    && let Some(SettingsField::Root(i)) = p.field_at(p.selected)
                {
                    p.remove_root(i);
                }
            }
            _ => {}
        }
    }

    /// Persist the settings pane, apply the changes, and close it.
    fn save_settings(&mut self, c: &mut MutexGuard<'_, PlayerController>) {
        let Some(pane) = self.settings.take() else {
            return;
        };
        let roots_changed = pane.roots_changed();

        if roots_changed {
            c.store.save_roots(&pane.roots);
        }
        c.store.save_scrobble_token(&pane.token);
        c.store.save_volume(pane.volume);
        c.store.save_replaygain(pane.replaygain);
        c.store.save_watch_roots(pane.watch_roots);
        c.store.save_live_columns(pane.live_columns);

        // Apply immediately: volume, replaygain and scrobbler.
        c.volume = pane.volume;
        c.replaygain = pane.replaygain;
        self.live_columns = pane.live_columns;

        if let Some(tx) = &self.watcher_tx {
            let _ = tx.send(if pane.watch_roots {
                pane.roots.clone()
            } else {
                Vec::new()
            });
        }

        if pane.token.is_empty() {
            c.set_scrobbler(Arc::new(NoopScrobbler));
        } else {
            c.set_scrobbler(Arc::new(ListenbrainzScrobbler::new(pane.token)));
        }

        // Rescan in the background if the roots changed.
        if roots_changed {
            self.spawn_scan(c.store.clone(), pane.roots.clone());
            self.status = "settings saved, rescanning...".into();
        } else {
            self.status = "settings saved".into();
        }
    }

    /// Spawn a background scan of `roots`, publishing live progress.
    fn spawn_scan(&mut self, store: Arc<LibraryStore>, roots: Vec<String>) {
        let progress = ScanProgress::new();
        self.scan_progress = Some(progress.clone());
        std::thread::spawn(move || {
            scan_roots(&store, &roots, &progress);
        });
    }

    fn jump_to_playing(&mut self, store: &LibraryStore) {
        self.follow = true;
        let Some(np) = &self.now_playing else {
            self.status = "nothing playing".into();
            return;
        };
        // A jump is an abrupt reposition, not a scroll — adopt the new
        // column widths immediately instead of waiting for the debounce.
        self.col_initialized = false;
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
                    // Track not in the filtered set (e.g. enqueued from
                    // another tab) or sort is random — leave the filter intact.
                    self.status = "playing track not in list".into();
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
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
                    e.tracks = tracks;
                    e.drilled = true;
                    e.selection = 0;
                    self.scroll_offset = 0;
                    self.follow = true;
                    self.status = "v: album tracks (v to collapse)".into();
                }
            } else {
                // Track level (or album kind) -> collapse.
                self.expanded = None;
                self.scroll_offset = 0;
                self.follow = true;
                self.status.clear();
            }
            return;
        }
        // Not expanded: expand.
        match self.tab {
            Tab::Albums => {
                if let Some(a) = self.albums.get(self.sel_albums).cloned() {
                    let tracks = c.store.album_tracks(&a.artist, &a.album);
                    let label = format!("{} - {}", a.artist, a.album);
                    self.expanded = Some(ExpandedView {
                        kind: ExpandKind::Album,
                        tracks,
                        albums: Vec::new(),
                        drilled: true,
                        selection: 0,
                        label,
                    });
                    self.scroll_offset = 0;
                    self.follow = true;
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
                    self.scroll_offset = 0;
                    self.follow = true;
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

/// Play track IDs in order: clear the queue, enqueue them, play the first.
fn play_sequence_ids(c: &mut MutexGuard<'_, PlayerController>, ids: Vec<String>) {
    let mut iter = ids.into_iter();
    if let Some(first) = iter.next() {
        c.dispatch(PlayerIntent::PlayTrack { id: first });
        for id in iter {
            c.dispatch(PlayerIntent::Enqueue { id, next: false });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> SettingsPane {
        SettingsPane::new(
            vec!["/music".to_string(), "/more".to_string()],
            "token".to_string(),
            0.8,
            false,
            false,
            false,
        )
    }

    #[test]
    fn field_layout() {
        let p = pane();
        // 2 roots + add + token + volume + replaygain + watch + live = 8 rows.
        assert_eq!(p.field_count(), 8);
        assert_eq!(p.field_at(0), Some(SettingsField::Root(0)));
        assert_eq!(p.field_at(1), Some(SettingsField::Root(1)));
        assert_eq!(p.field_at(2), Some(SettingsField::AddRoot));
        assert_eq!(p.field_at(3), Some(SettingsField::Token));
        assert_eq!(p.field_at(4), Some(SettingsField::Volume));
        assert_eq!(p.field_at(5), Some(SettingsField::ReplayGain));
        assert_eq!(p.field_at(6), Some(SettingsField::WatchRoots));
        assert_eq!(p.field_at(7), Some(SettingsField::LiveColumns));
        assert_eq!(p.field_at(8), None);
    }

    #[test]
    fn edit_root_updates() {
        let mut p = pane();
        p.selected = 0;
        p.begin_edit(SettingsField::Root(0));
        p.edit_buf = "/new".to_string();
        p.commit_edit();
        assert_eq!(p.roots[0], "/new");
        assert!(p.roots_changed());
    }

    #[test]
    fn edit_root_empty_removes() {
        let mut p = pane();
        p.selected = 0;
        p.begin_edit(SettingsField::Root(0));
        p.edit_buf = "   ".to_string();
        p.commit_edit();
        assert_eq!(p.roots, vec!["/more".to_string()]);
        // Selection clamped into range.
        assert!(p.selected < p.field_count());
    }

    #[test]
    fn add_root_appends() {
        let mut p = pane();
        p.selected = 2;
        p.begin_edit(SettingsField::AddRoot);
        p.edit_buf = "/extra".to_string();
        p.commit_edit();
        assert_eq!(p.roots.len(), 3);
        assert_eq!(p.roots[2], "/extra");
        assert!(p.roots_changed());
    }

    #[test]
    fn remove_root() {
        let mut p = pane();
        // Remove the first root; selection moves to the next row.
        p.selected = 0;
        p.remove_root(0);
        assert_eq!(p.roots, vec!["/more".to_string()]);
        assert_eq!(p.selected, 0);
        assert!(p.roots_changed());
        // Remove the last root; selection clamps to the final row.
        p.selected = 0;
        p.remove_root(0);
        assert!(p.roots.is_empty());
        assert!(p.selected < p.field_count());
    }

    #[test]
    fn add_root_empty_is_noop() {
        let mut p = pane();
        p.selected = 2;
        p.begin_edit(SettingsField::AddRoot);
        p.edit_buf = "".to_string();
        p.commit_edit();
        assert_eq!(p.roots.len(), 2);
        assert!(!p.roots_changed());
    }

    #[test]
    fn edit_token_and_volume() {
        let mut p = pane();
        p.selected = 3;
        p.begin_edit(SettingsField::Token);
        p.edit_buf = "  newtoken  ".to_string();
        p.commit_edit();
        assert_eq!(p.token, "newtoken");

        p.selected = 4;
        p.begin_edit(SettingsField::Volume);
        p.edit_buf = "150".to_string(); // clamped to 100
        p.commit_edit();
        assert!((p.volume - 1.0).abs() < f32::EPSILON);

        p.begin_edit(SettingsField::Volume);
        p.edit_buf = "abc".to_string(); // invalid: unchanged
        p.commit_edit();
        assert!((p.volume - 1.0).abs() < f32::EPSILON);
    }
}
