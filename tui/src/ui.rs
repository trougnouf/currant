// ./tui/src/ui.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering. Reads the `App` view model (built each frame) and lays out the
//! tabs, the active list and the now-playing bar.

use crate::app::{
    App, ColumnWidths, ExpandKind, QueueKind, SettingsPane, Tab, ViewPreset, col_num, col_text,
    disp_width, display_title, fmt_duration, render_rating, truncate,
};
use cassis_core::model::Track;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState, Padding, Paragraph, Tabs,
};

/// Color palette for the TUI.
mod theme {
    use ratatui::style::Color;
    pub const ACCENT: Color = Color::Cyan;
    pub const ACCENT_DIM: Color = Color::DarkGray;
    pub const TITLE: Color = Color::Yellow;
    pub const NOW_PLAYING: Color = Color::Green;
    pub const PAUSED: Color = Color::Yellow;
    pub const STOP_AFTER: Color = Color::Red;
    pub const RATING: Color = Color::Magenta;
    pub const DURATION: Color = Color::DarkGray;
    pub const ARTIST: Color = Color::Blue;
    pub const ALBUM: Color = Color::Cyan;
    pub const YEAR: Color = Color::DarkGray;
    pub const GENRE: Color = Color::Green;
    pub const TRACK: Color = Color::DarkGray;
    pub const PATH: Color = Color::DarkGray;
    pub const QUEUE_NOW: Color = Color::Green;
    pub const QUEUE_EXPLICIT: Color = Color::Yellow;
    pub const QUEUE_DYNAMIC: Color = Color::DarkGray;
    pub const GAUGE: Color = Color::Cyan;
    pub const HINT: Color = Color::DarkGray;
    pub const POPUP_BORDER: Color = Color::Cyan;
}

use std::sync::OnceLock;

/// Pick the display name once per process so it stays stable within a session
/// but varies between launches.
fn display_name() -> &'static str {
    static NAME: OnceLock<&str> = OnceLock::new();
    NAME.get_or_init(|| {
        if fastrand::bool() {
            "Cassis"
        } else {
            "Currant"
        }
    })
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tabs + search
            Constraint::Min(0),    // list
            Constraint::Length(5), // now playing + progress + status
        ])
        .split(f.area());

    app.sync_scroll(chunks[1].height);

    draw_header(f, app, chunks[0]);
    draw_list(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.help {
        let area = centered(f.area(), 70, 80);
        f.render_widget(Clear, area);
        draw_help(f, area, app);
    }
    if let Some(track) = &app.details {
        let area = centered(f.area(), 70, 60);
        f.render_widget(Clear, area);
        draw_details(f, area, track);
    }
    if let Some(pane) = &app.settings {
        let area = centered(f.area(), 70, 60);
        f.render_widget(Clear, area);
        draw_settings(f, area, pane);
    }
}

/// Set the ListState selection and offset from the App's scroll_offset.
/// The offset is clamped so the selection stays visible.
fn apply_scroll(state: &mut ListState, selected: usize, total: usize, area: Rect, offset: usize) {
    state.select(Some(selected));
    let visible = area.height.saturating_sub(2) as usize; // borders
    let max_offset = total.saturating_sub(visible);
    *state.offset_mut() = offset.min(max_offset);
}

/// A centered rect inside `area` with the given width/height percentages.
fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(popup[1])[1]
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(60), Constraint::Min(0)])
        .split(area);

    let titles = ["tracks", "albums", "artists", "queue", "playlists", "files"]
        .iter()
        .map(|t| Line::from(*t))
        .collect::<Vec<_>>();
    let active = match app.tab {
        Tab::Tracks => 0,
        Tab::Albums => 1,
        Tab::Artists => 2,
        Tab::Queue => 3,
        Tab::Playlists => 4,
        Tab::Files => 5,
    };
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(display_name())
                .title_style(Style::default().fg(theme::TITLE).bold()),
        )
        .select(active)
        .style(Style::default())
        .highlight_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, cols[0]);

    let prompt = if app.in_search {
        "search>"
    } else {
        "search ('/ to type)"
    };
    let search = Paragraph::new(format!("{} {}", prompt, app.search)).block(
        Block::default()
            .borders(Borders::ALL)
            .title("filter")
            .title_style(Style::default().fg(theme::TITLE)),
    );
    f.render_widget(search, cols[1]);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    if let Some(e) = app.expanded_view() {
        draw_expanded(f, app, area, e);
        return;
    }
    match app.tab {
        Tab::Tracks => draw_tracks(f, app, area, app.tracks_view(), "tracks"),
        Tab::Files => draw_tracks(f, app, area, app.files_view(), "files"),
        Tab::Albums => draw_albums(f, app, area),
        Tab::Artists => draw_artists(f, app, area),
        Tab::Queue => draw_queue(f, app, area),
        Tab::Playlists => draw_playlists(f, app, area),
    }
}

fn draw_expanded(f: &mut Frame, app: &App, area: Rect, e: &crate::app::ExpandedView) {
    if !e.drilled && e.kind == ExpandKind::Artist {
        draw_expanded_albums(f, app, area, e);
        return;
    }
    let playing_id = app.now_playing.as_ref().map(|t| &t.id);
    let stop_id = app.stop_after.as_deref();
    let width = area.width.saturating_sub(7) as usize;
    let cols = app.col_widths();
    let rows: Vec<ListItem> = e
        .tracks
        .iter()
        .map(|t| {
            ListItem::new(track_line(
                t,
                app.view,
                playing_id == Some(&t.id),
                stop_id == Some(t.id.as_str()),
                &cols,
                width,
            ))
        })
        .collect();
    let prefix = match e.kind {
        ExpandKind::Album => "album tracks",
        ExpandKind::Artist => "artist album tracks",
    };
    let title = format!(
        "{prefix}: {} - {} tracks (v: collapse)",
        e.label,
        e.tracks.len()
    );
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(
        &mut state,
        e.selection,
        e.tracks.len(),
        area,
        app.scroll_offset(),
    );
    f.render_stateful_widget(list, area, &mut state);
}

/// Album list within an expanded artist.
fn draw_expanded_albums(f: &mut Frame, app: &App, area: Rect, e: &crate::app::ExpandedView) {
    let rows: Vec<ListItem> = e
        .albums
        .iter()
        .map(|a| {
            let year = if a.year > 0 {
                format!(" [{}] ", a.year)
            } else {
                " ".to_string()
            };
            ListItem::new(format!(
                "{}{}({} tracks, {})",
                a.album,
                year,
                a.track_count,
                fmt_duration(a.total_duration_secs)
            ))
        })
        .collect();
    let title = format!(
        "artist albums: {} - {} albums (v: expand  Enter: play)",
        e.label,
        e.albums.len()
    );
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(
        &mut state,
        e.selection,
        e.albums.len(),
        area,
        app.scroll_offset(),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_tracks(
    f: &mut Frame,
    app: &App,
    area: Rect,
    (items, selected, total, offset): (&[Track], usize, u64, usize),
    title: &str,
) {
    let title = format!("{title} - {} of {} (from #{})", items.len(), total, offset);
    let playing_id = app.now_playing.as_ref().map(|t| &t.id);
    let stop_id = app.stop_after.as_deref();
    let width = area.width.saturating_sub(6) as usize; // borders + highlight + markers
    let cols = app.col_widths();
    let rows: Vec<ListItem> = items
        .iter()
        .map(|t| {
            ListItem::new(track_line(
                t,
                app.view,
                playing_id == Some(&t.id),
                stop_id == Some(t.id.as_str()),
                &cols,
                width,
            ))
        })
        .collect();
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(
        &mut state,
        selected,
        total as usize,
        area,
        app.scroll_offset(),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn track_line<'a>(
    t: &'a Track,
    view: ViewPreset,
    playing: bool,
    stop_after: bool,
    cols: &ColumnWidths,
    width: usize,
) -> Line<'a> {
    let playing_marker = if playing { ">" } else { " " };
    let playing_style = if playing {
        Style::default().fg(theme::NOW_PLAYING).bold()
    } else {
        Style::default()
    };
    let stop_marker = if stop_after { "|" } else { " " };
    let stop_style = if stop_after {
        Style::default().fg(theme::STOP_AFTER).bold()
    } else {
        Style::default()
    };
    let rating = render_rating(t.rating);
    let rating_style = Style::default().fg(theme::RATING);
    let title = display_title(t);
    let title_style = if playing {
        Style::default().fg(theme::NOW_PLAYING).bold()
    } else {
        Style::default().bold()
    };
    let dur = fmt_duration(t.duration_secs);
    let dur_style = Style::default().fg(theme::DURATION);
    let artist_style = Style::default().fg(theme::ARTIST);
    let album_style = Style::default().fg(theme::ALBUM);
    let year_style = Style::default().fg(theme::YEAR);
    let genre_style = Style::default().fg(theme::GENRE);
    let track_style = Style::default().fg(theme::TRACK);

    let track_no = if t.track_number > 0 {
        format!("{:02}. ", t.track_number)
    } else {
        String::new()
    };

    // Build column spans: playing marker, stop marker, separator, artist,
    // [album], title (track_no + title), [year], [genre], duration. Single-space
    // separators between columns.
    let mut spans = vec![
        Span::styled(playing_marker, playing_style),
        Span::styled(stop_marker, stop_style),
        Span::raw(" "),
    ];

    // Artist (left-aligned).
    spans.push(Span::styled(col_text(&t.artist, cols.artist), artist_style));

    // Album (left-aligned, compact and full only).
    if view != ViewPreset::Minimal {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(col_text(&t.album, cols.album), album_style));
    }

    // Title column: track_no prefix + title, left-aligned as a unit.
    // track_no keeps its own style; title is truncated/padded to fill.
    spans.push(Span::raw(" "));
    let track_no_len = disp_width(&track_no);
    let title_avail = cols.title.saturating_sub(track_no_len);
    let title_disp = if disp_width(&title) <= title_avail {
        title.clone()
    } else {
        truncate(title, title_avail)
    };
    let title_pad = cols
        .title
        .saturating_sub(track_no_len + disp_width(&title_disp));
    spans.push(Span::styled(track_no, track_style));
    spans.push(Span::styled(title_disp, title_style));
    spans.push(Span::raw(" ".repeat(title_pad)));

    // Year (right-aligned, full only).
    if view == ViewPreset::Full {
        spans.push(Span::raw(" "));
        let year_str = if t.year > 0 {
            t.year.to_string()
        } else {
            String::new()
        };
        spans.push(Span::styled(col_num(&year_str, cols.year), year_style));
    }

    // Genre (left-aligned, full only).
    if view == ViewPreset::Full {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(col_text(&t.genre, cols.genre), genre_style));
    }

    // Duration (right-aligned).
    spans.push(Span::raw(" "));
    spans.push(Span::styled(col_num(&dur, cols.duration), dur_style));

    // Right-aligned rating.
    let left_len: usize = spans.iter().map(|s| disp_width(&s.content)).sum();
    let rating_len = disp_width(&rating) + 1;
    let pad = width.saturating_sub(left_len + rating_len);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(rating, rating_style));

    Line::from(spans)
}

fn draw_albums(f: &mut Frame, app: &App, area: Rect) {
    let (albums, selected) = app.albums_view();
    let rows: Vec<ListItem> = albums
        .iter()
        .map(|a| {
            let year = if a.year > 0 {
                format!(" [{}] ", a.year)
            } else {
                " ".to_string()
            };
            ListItem::new(format!(
                "{} - {}{}({} tracks, {})",
                a.artist,
                a.album,
                year,
                a.track_count,
                fmt_duration(a.total_duration_secs)
            ))
        })
        .collect();
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("albums - {}", albums.len()))
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(
        &mut state,
        selected,
        albums.len(),
        area,
        app.scroll_offset(),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_artists(f: &mut Frame, app: &App, area: Rect) {
    let (artists, selected) = app.artists_view();
    let rows: Vec<ListItem> = artists
        .iter()
        .map(|a| {
            ListItem::new(format!(
                "{} ({} albums, {} tracks)",
                a.name, a.album_count, a.track_count
            ))
        })
        .collect();
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("artists - {}", artists.len()))
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(
        &mut state,
        selected,
        artists.len(),
        area,
        app.scroll_offset(),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_queue(f: &mut Frame, app: &App, area: Rect) {
    let (rows, selected) = app.queue_view();
    let stop_id = app.stop_after.as_deref();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let (mark, color) = match r.kind {
                QueueKind::NowPlaying => (">", theme::QUEUE_NOW),
                QueueKind::Explicit => ("+", theme::QUEUE_EXPLICIT),
                QueueKind::Dynamic => ("~", theme::QUEUE_DYNAMIC),
            };
            let stop = if stop_id == Some(r.id.as_str()) {
                Span::styled("|", Style::default().fg(theme::STOP_AFTER).bold())
            } else {
                Span::raw(" ")
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color).bold()),
                stop,
                Span::raw(" "),
                Span::styled(r.artist.clone(), Style::default().fg(theme::ARTIST)),
                Span::raw(" - "),
                Span::raw(r.title.clone()),
            ])
            .into()
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("queue - {} (Enter:jump to  x:remove)", rows.len()))
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(&mut state, selected, rows.len(), area, app.scroll_offset());
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_playlists(f: &mut Frame, app: &App, area: Rect) {
    let (playlists, selected) = app.playlists_view();
    let items: Vec<ListItem> = playlists
        .iter()
        .enumerate()
        .map(|(i, pl)| {
            ListItem::new(format!(
                "g{}  {}  [{}]  sort: {}",
                i + 1,
                pl.name,
                pl.query,
                crate::app::sort_label(pl.sort_preset)
            ))
        })
        .collect();
    let title = if playlists.is_empty() {
        "playlists - empty (P to save current search)".to_string()
    } else {
        format!(
            "playlists - {} (Enter: activate  x: delete)",
            playlists.len()
        )
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    apply_scroll(
        &mut state,
        selected,
        playlists.len(),
        area,
        app.scroll_offset(),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Length(1)])
        .split(area);

    let np_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(chunks[0].inner(Margin::new(1, 1)));

    let pos_ms = app.position_ms();
    let duration_secs = app
        .now_playing
        .as_ref()
        .map(|t| t.duration_secs)
        .unwrap_or(0);
    let pos_secs = (pos_ms / 1000) as u32;
    let pct = if duration_secs > 0 {
        (pos_ms as f64 / (duration_secs as f64 * 1000.0) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let np_line = if let Some(t) = &app.now_playing {
        let status_color = if app.is_playing {
            theme::NOW_PLAYING
        } else {
            theme::PAUSED
        };
        let stop = if app.stop_after.is_some() {
            " [stop after]"
        } else {
            ""
        };
        vec![
            Span::styled(
                format!("{}: ", if app.is_playing { "playing" } else { "paused" }),
                Style::default().fg(status_color).bold(),
            ),
            Span::styled(t.title.clone(), Style::default().bold()),
            Span::raw(" - "),
            Span::styled(t.artist.clone(), Style::default().fg(theme::ARTIST)),
            Span::raw(" ["),
            Span::styled(t.album.clone(), Style::default().fg(theme::ALBUM)),
            Span::raw("] "),
            Span::styled(render_rating(t.rating), Style::default().fg(theme::RATING)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{}/{}",
                    fmt_duration(pos_secs),
                    fmt_duration(t.duration_secs)
                ),
                Style::default().fg(theme::DURATION),
            ),
            Span::styled(stop, Style::default().fg(theme::STOP_AFTER).bold()),
        ]
    } else {
        vec![Span::raw("stopped. press Enter on a track to play.")]
    };
    let radio = app
        .radio_sort
        .map(|s| match s {
            cassis_core::model::SortPreset::RandomAlbum => "random album",
            cassis_core::model::SortPreset::Random => "random",
            _ => "off",
        })
        .unwrap_or("-");
    let vol = format!("vol: {:0.0}%", app.volume * 100.0);
    let mut line = np_line;
    line.push(Span::raw("  | "));
    line.push(Span::styled(
        format!("radio: {radio}"),
        Style::default().fg(theme::ACCENT),
    ));
    line.push(Span::raw("  | "));
    line.push(Span::styled(vol, Style::default().fg(theme::ACCENT)));
    let paragraph = Paragraph::new(Line::from(line)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("now playing - {} tracks", app.track_count))
            .title_style(Style::default().fg(theme::TITLE)),
    );
    f.render_widget(paragraph, chunks[0]);
    let gauge_label = format!(
        "{} / {}",
        fmt_duration(pos_secs),
        fmt_duration(duration_secs)
    );
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(theme::GAUGE).bg(theme::ACCENT_DIM))
            .percent(pct as u16)
            .label(gauge_label),
        np_area[1],
    );

    let status = if app.status.is_empty() {
        tab_hint(app.tab)
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(theme::HINT)),
        chunks[1],
    );
}

/// Context-sensitive keybinding hint for the bottom status line.
fn tab_hint(tab: Tab) -> String {
    let universal = "  Tab:tabs F1-F6:jump  /:search  p:play  n:next <:prev  N:skip-album  +/-:vol  h/l:seek  v:expand  f:play-next e:queue  o:settings  ?:help  q:quit  Ctrl+J:jump to playing";
    let actions = match tab {
        Tab::Tracks | Tab::Files => {
            "Enter:play  f:play-next  e:queue  x:remove  0-5:rate  d:details  c:view  s:sort  r:radio  S:stop-after"
        }
        Tab::Albums => {
            "Enter:play album  v:expand  f:play-next  e:queue album  d:details  c:view  s:sort  r:radio"
        }
        Tab::Artists => {
            "Enter:play artist  v:expand  f:play-next  e:queue artist  d:details  c:view  s:sort  r:radio"
        }
        Tab::Queue => "Enter:jump to  x:remove  S:stop-after  d:details  c:view",
        Tab::Playlists => "Enter:activate  x:delete  P:save current search as playlist",
    };
    format!("{actions}{universal}")
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("help")
        .title_style(Style::default().fg(theme::TITLE))
        .border_style(Style::default().fg(theme::POPUP_BORDER))
        .padding(Padding::new(2, 2, 1, 1));
    let mut text = String::new();
    let bindings: [(&str, &str); 27] = [
        ("/search", "filter the current tab (Esc to leave)"),
        ("Tab / Shift+Tab", "switch tabs"),
        (
            "F1-F6",
            "jump to tab (Tracks/Albums/Artists/Queue/Playlists/Files)",
        ),
        ("j k / arrows", "move selection (PgUp/PgDn jump 20)"),
        ("Enter", "play / activate playlist (playlists tab)"),
        ("v", "expand album/artist (v again to collapse)"),
        ("e", "enqueue (append to queue)"),
        ("f", "play next (front of queue)"),
        ("x", "remove from queue / delete playlist"),
        ("S", "stop after selected track (toggle)"),
        ("0-5", "rate track (0 clears)"),
        ("c", "cycle columns (minimal / compact / full)"),
        ("s", "cycle sort"),
        ("r / R", "toggle radio (random / random-album)"),
        ("p", "play / pause"),
        ("n > .", "next track"),
        ("N", "skip album"),
        ("< ,", "previous track"),
        ("+ -", "volume up / down"),
        ("</> (h/l)", "seek backward / forward 5s"),
        ("H L", "seek backward / forward 30s"),
        ("P", "save current search as a smart playlist"),
        ("g1-9", "activate saved playlist by index"),
        ("d", "show track details (path, metadata, etc.)"),
        (
            "o",
            "open settings (scan roots, listenbrainz token, volume)",
        ),
        ("Ctrl+J", "jump to currently playing track in the list"),
        ("q", "quit (Esc closes overlays)"),
    ];
    for (key, desc) in bindings {
        text.push_str(&format!("{key:<20} {desc}\n"));
    }
    text.push_str("\nmouse:\n");
    text.push_str("  click tab        switch tabs\n");
    text.push_str("  click list       select (double-click: play)\n");
    text.push_str("  scroll           move selection\n");
    text.push_str("  click progress   seek\n");
    text.push_str("  shift+drag       terminal text selection (copy)\n");
    text.push_str("\nsearch syntax:\n");
    let syntax: [(&str, &str); 13] = [
        ("free text", "matches title, artist, album, comment"),
        ("ar:pink / artist:pink", "artist contains 'pink'"),
        ("al:=blue / album:=blue", "album equals exactly"),
        ("t:-love / title:-love", "title does not contain 'love'"),
        ("#jazz / genre:jazz", "genre contains 'jazz'"),
        ("c:notes / comment:notes", "comment contains 'notes'"),
        ("year:>=1990", "year >= 1990"),
        ("*>=4 / rating:>=4", "rating >= 4 stars"),
        ("~>5m / length:>5m", "duration > 5 minutes"),
        ("p:0 / playcount:0", "play count = 0 (never played)"),
        ("-term", "exclude (NOT)"),
        ("a | b", "either (OR)"),
        ("(a b)", "grouping (implicit AND)"),
    ];
    for (key, desc) in syntax {
        text.push_str(&format!("  {key:<24} {desc}\n"));
    }
    if !app.smart_playlists.is_empty() {
        text.push_str("\nsaved playlists (g+N or playlists tab):\n");
        for (i, pl) in app.smart_playlists.iter().take(9).enumerate() {
            text.push_str(&format!("  g{}  {}  [{}]\n", i + 1, pl.name, pl.query));
        }
    }
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn draw_details(f: &mut Frame, area: Rect, track: &Track) {
    let label = Style::default().fg(theme::ACCENT);
    let val = Style::default();
    let lines = vec![
        Line::from(vec![
            Span::styled("title:  ", label),
            Span::styled(&track.title, val),
        ]),
        Line::from(vec![
            Span::styled("artist: ", label),
            Span::styled(&track.artist, val),
        ]),
        Line::from(vec![
            Span::styled("album:  ", label),
            Span::styled(&track.album, val),
        ]),
        Line::from(vec![
            Span::styled("album_artist: ", label),
            Span::styled(&track.album_artist, val),
        ]),
        Line::from(vec![
            Span::styled("genre:  ", label),
            Span::styled(&track.genre, val),
        ]),
        Line::from(vec![
            Span::styled("comment: ", label),
            Span::styled(&track.comment, val),
        ]),
        Line::from(vec![
            Span::styled("track_number: ", label),
            Span::styled(track.track_number.to_string(), val),
            Span::styled("    year: ", label),
            Span::styled(
                if track.year > 0 {
                    track.year.to_string()
                } else {
                    "?".into()
                },
                val,
            ),
            Span::styled("    duration: ", label),
            Span::styled(fmt_duration(track.duration_secs), val),
        ]),
        Line::from(vec![
            Span::styled("rating: ", label),
            Span::styled(
                render_rating(track.rating),
                Style::default().fg(theme::RATING),
            ),
            Span::styled("    play_count: ", label),
            Span::styled(track.play_count.to_string(), val),
            Span::styled("    last_played: ", label),
            Span::styled(
                track
                    .last_played
                    .map(|ts| format!("unix:{ts}"))
                    .unwrap_or("never".into()),
                val,
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("path: ", label),
            Span::styled(&track.path, Style::default().fg(theme::PATH)),
        ]),
        Line::from(vec![
            Span::styled("id:   ", label),
            Span::styled(&track.id, val),
        ]),
        Line::from(vec![
            Span::styled("mtime: ", label),
            Span::styled(track.file_mtime.to_string(), val),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("track details (any key to close)")
        .title_style(Style::default().fg(theme::TITLE))
        .border_style(Style::default().fg(theme::POPUP_BORDER))
        .padding(Padding::new(2, 2, 1, 1));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The settings overlay: a list of editable fields (scan roots, scrobble
/// token, default volume). When a field is being edited, an input line shows
/// the buffer being typed.
fn draw_settings(f: &mut Frame, area: Rect, pane: &SettingsPane) {
    let editing = pane.editing.is_some();
    let mut constraints = vec![Constraint::Min(0)];
    if editing {
        constraints.push(Constraint::Length(3));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let rows: Vec<ListItem> = (0..pane.field_count())
        .map(|i| {
            let field = pane.field_at(i).unwrap();
            let (label, value) = pane.field_text(&field);
            let line = Line::from(vec![
                Span::styled(format!("{label:<20}"), Style::default().fg(theme::ACCENT)),
                Span::styled(value, Style::default()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("settings (Enter/Space: edit·toggle  x: remove root  Esc: save & close)")
                .title_style(Style::default().fg(theme::TITLE))
                .border_style(Style::default().fg(theme::POPUP_BORDER)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");

    let mut state = ListState::default();
    state.select(Some(pane.selected));
    f.render_stateful_widget(list, chunks[0], &mut state);

    if editing {
        let input = Paragraph::new(format!("{}|", pane.edit_buf)).block(
            Block::default()
                .borders(Borders::ALL)
                .title("value (Enter: save, Esc: cancel)")
                .title_style(Style::default().fg(theme::TITLE))
                .border_style(Style::default().fg(theme::POPUP_BORDER)),
        );
        f.render_widget(input, chunks[1]);
    }
}
